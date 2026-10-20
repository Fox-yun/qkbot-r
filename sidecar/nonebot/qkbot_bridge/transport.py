from __future__ import annotations

import asyncio
import base64
import binascii
import json
import mimetypes
import os
import re
import stat
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator
from urllib.parse import urlsplit
from urllib.request import url2pathname

import aiohttp

BODY_LIMIT = 1024 * 1024
IDENTIFIER = re.compile(r"nb_[A-Za-z0-9_]{1,61}\Z")
MODULE = re.compile(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*\Z", re.ASCII)


class BridgeError(RuntimeError):
    """A deliberately small, credential-free error crossing the plugin boundary."""


@dataclass(frozen=True)
class Settings:
    url: str
    token: str = field(repr=False)
    allowed_modules: frozenset[str] = frozenset({"qkbot_echo"})
    media_roots: tuple[Path, ...] = ()
    media_max_bytes: int = 20 * 1024 * 1024
    concurrency: int = 8
    task_timeout: float = 120
    http_timeout: float = 30

    def __post_init__(self) -> None:
        address = urlsplit(self.url)
        if (
            address.scheme != "http"
            or address.hostname != "127.0.0.1"
            or not address.port
            or address.path not in ("", "/")
            or address.username
            or address.password
            or address.query
            or address.fragment
        ):
            raise BridgeError("bridge URL must be http://127.0.0.1:PORT")
        if not 32 <= len(self.token) <= 4096 or any(
            ord(char) < 33 or ord(char) > 126 for char in self.token
        ):
            raise BridgeError("bridge token must contain 32..4096 visible ASCII characters")
        if not 1 <= self.concurrency <= 32:
            raise BridgeError("event concurrency must be 1..32")
        if not 10 <= self.task_timeout <= 600 or not 1 <= self.http_timeout <= 120:
            raise BridgeError("bridge timeout is outside the supported range")
        if not 1024 <= self.media_max_bytes <= 1024 * 1024 * 1024:
            raise BridgeError("media size limit must be 1 KiB..1 GiB")
        if not self.allowed_modules or any(not MODULE.fullmatch(m) for m in self.allowed_modules):
            raise BridgeError("invalid plugin module allow list")


class Transport:
    """Bounded loopback transport. A failed POST is never automatically retried."""

    def __init__(self, settings: Settings):
        self.settings = settings
        self._session: aiohttp.ClientSession | None = None

    async def session(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            trace = aiohttp.TraceConfig()

            async def reject_redirect(*_: Any) -> None:
                raise BridgeError("bridge redirects are not allowed")

            trace.on_request_redirect.append(reject_redirect)
            self._session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=self.settings.http_timeout),
                headers={"Authorization": f"Bearer {self.settings.token}"},
                trust_env=False,
                trace_configs=[trace],
                connector=aiohttp.TCPConnector(limit=self.settings.concurrency + 2),
            )
        return self._session

    async def close(self) -> None:
        if self._session is not None:
            await self._session.close()

    async def request(self, method: str, path: str, **kwargs: Any) -> Any:
        if not path.startswith("/") or path.startswith("//"):
            raise BridgeError("invalid bridge route")
        try:
            client = await self.session()
            async with client.request(
                method,
                self.settings.url.rstrip("/") + path,
                allow_redirects=False,
                **kwargs,
            ) as response:
                body = bytearray()
                async for chunk in response.content.iter_chunked(16384):
                    body.extend(chunk)
                    if len(body) > BODY_LIMIT:
                        raise BridgeError("bridge response exceeds 1 MiB")
                if response.status != 200:
                    raise BridgeError(f"bridge refused request (HTTP {response.status})")
                try:
                    return json.loads(body)
                except (ValueError, UnicodeDecodeError) as error:
                    raise BridgeError("bridge returned invalid JSON") from error
        except (aiohttp.ClientError, TimeoutError) as error:
            # Do not include URLs, response bodies, request arguments or auth headers.
            raise BridgeError("bridge transport failed; request was not retried") from error

    async def catalog(self) -> dict[str, str]:
        data = await self.request("GET", "/qkbot/catalog")
        entries = data.get("plugins") if isinstance(data, dict) else None
        if not isinstance(entries, list) or len(entries) > 32:
            raise BridgeError("invalid plugin catalog")
        bindings: dict[str, str] = {}
        ids: set[str] = set()
        for entry in entries:
            if not isinstance(entry, dict):
                raise BridgeError("invalid plugin catalog entry")
            if entry.get("enabled") is False:
                continue
            module, plugin_id = entry.get("module"), entry.get("id")
            if (
                not isinstance(module, str)
                or not MODULE.fullmatch(module)
                or not isinstance(plugin_id, str)
                or not IDENTIFIER.fullmatch(plugin_id)
                or module in bindings
                or plugin_id in ids
            ):
                raise BridgeError("invalid or duplicate plugin catalog binding")
            if module not in self.settings.allowed_modules:
                raise BridgeError("enabled catalog plugin is absent from the local module allow list")
            bindings[module] = plugin_id
            ids.add(plugin_id)
        return bindings

    async def upload(self, uri: str, identity: dict[str, Any]) -> str:
        """Accept only bytes or an explicitly approved local media root."""
        size: int
        stream: Any
        close = None
        if uri.startswith("base64://"):
            encoded = uri[len("base64://") :]
            if len(encoded) > (self.settings.media_max_bytes + 2) // 3 * 4:
                raise BridgeError("media exceeds configured size limit")
            try:
                stream = base64.b64decode(encoded, validate=True)
            except (ValueError, binascii.Error) as error:
                raise BridgeError("invalid base64 media") from error
            size, prefix = len(stream), stream[:32]
            content_type = detect_media(prefix)
        elif uri.startswith("file://"):
            parsed = urlsplit(uri)
            if parsed.netloc not in ("", "localhost") or parsed.query or parsed.fragment:
                raise BridgeError("remote or ambiguous file URI is not allowed")
            try:
                # url2pathname already decodes percent escapes once. A literal
                # "%20" filename must not silently become a space on a second pass.
                path = Path(url2pathname(parsed.path)).resolve(strict=True)
                roots = tuple(root.resolve(strict=True) for root in self.settings.media_roots)
                if not any(path.is_relative_to(root) for root in roots):
                    raise BridgeError("media file is outside the configured roots")
                handle = path.open("rb")
                close = handle.close
                metadata = await asyncio.to_thread(os.fstat, handle.fileno())
                if not stat.S_ISREG(metadata.st_mode):
                    raise BridgeError("media source is not a regular file")
                size = metadata.st_size
                prefix = await asyncio.to_thread(handle.read, 32)
                await asyncio.to_thread(handle.seek, 0)
                content_type = detect_media(prefix, path.name)

                async def chunks() -> AsyncIterator[bytes]:
                    total = 0
                    while chunk := await asyncio.to_thread(handle.read, 65536):
                        total += len(chunk)
                        if total > size or total > self.settings.media_max_bytes:
                            raise BridgeError("media changed or exceeded configured size limit")
                        yield chunk
                    if total != size:
                        raise BridgeError("media changed during upload")

                stream = chunks()
            except (OSError, ValueError) as error:
                if close:
                    close()
                raise BridgeError("media file cannot be opened") from error
            except BaseException:
                if close:
                    close()
                raise
        else:
            raise BridgeError("unsupported local media URI")
        try:
            if not 0 < size <= self.settings.media_max_bytes:
                raise BridgeError("media is empty or exceeds configured size limit")
            result = await self.request(
                "POST",
                "/qkbot/media",
                data=stream,
                timeout=aiohttp.ClientTimeout(total=min(self.settings.task_timeout, 300)),
                headers={
                    "Content-Type": content_type,
                    "Content-Length": str(size),
                    "X-Qkbot-Task": identity["task_id"],
                    "X-Qkbot-Plugin": identity["plugin_id"],
                    "X-Qkbot-Generation": str(identity["generation"]),
                },
            )
            if not isinstance(result, dict) or not isinstance(result.get("url"), str):
                raise BridgeError("invalid media upload result")
            url = result["url"]
            if not url.startswith(self.settings.url.rstrip("/") + "/media/"):
                raise BridgeError("media upload returned a foreign URL")
            return url
        finally:
            if close:
                close()


def detect_media(prefix: bytes, name: str = "") -> str:
    if prefix.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if prefix.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if prefix[:6] in (b"GIF87a", b"GIF89a"):
        return "image/gif"
    if prefix[:4] == b"RIFF" and prefix[8:12] == b"WEBP":
        return "image/webp"
    if prefix[:4] == b"RIFF" and prefix[8:12] == b"WAVE":
        return "audio/wav"
    if prefix.startswith(b"OggS"):
        return "audio/ogg"
    if prefix.startswith((b"#!AMR", b"\x02#!SILK", b"#!SILK")):
        return "audio/amr" if prefix.startswith(b"#!AMR") else "audio/silk"
    if prefix.startswith((b"ID3", b"\xff\xfb", b"\xff\xf3", b"\xff\xf2")):
        return "audio/mpeg"
    if len(prefix) > 1 and prefix[0] == 0xff and prefix[1] & 0xf6 == 0xf0:
        return "audio/aac"
    if prefix.startswith(b"fLaC"):
        return "audio/flac"
    if prefix[4:8] == b"ftyp":
        guessed = mimetypes.guess_type(name)[0]
        return guessed if guessed in ("audio/mp4", "video/mp4") else "video/mp4"
    raise BridgeError("unrecognized media format")
