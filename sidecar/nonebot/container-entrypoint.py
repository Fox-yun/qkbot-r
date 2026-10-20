"""Container-only setup, then exec the same audited sidecar launcher."""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Mapping


PROFILES = {
    "core": (),
    "parser": ("parser-bilibili",),
    "parser-full": ("parser-full",),
    "download": ("download",),
    "web": ("web",),
    "music": ("music",),
    "full": ("full",),
}


def bridge_token(environ: Mapping[str, str]) -> str:
    value = environ.get("QKBOT_NONEBOT_TOKEN", "")
    explicit_file = environ.get("QKBOT_NONEBOT_TOKEN_FILE", "")
    path = Path(explicit_file or "/run/secrets/qkbot-nonebot-token")
    if explicit_file or (not value and path.is_file()):
        if value:
            raise ValueError("set either the bridge token or its file, not both")
        try:
            # Only one optional final newline is accepted; never silently trim
            # spaces inside a malformed credential or read an unbounded file.
            with path.open("rb") as source:
                raw = source.read(4099)
            if raw.endswith(b"\r\n"):
                raw = raw[:-2]
            elif raw.endswith(b"\n"):
                raw = raw[:-1]
            value = raw.decode("ascii")
        except (OSError, UnicodeError):
            raise ValueError("cannot read the bridge token file") from None
    if not 32 <= len(value) <= 4096 or any(not 33 <= ord(c) <= 126 for c in value):
        raise ValueError("bridge token must contain 32..4096 visible ASCII characters")
    return value


def command(environ: Mapping[str, str], extra: list[str], script: Path) -> list[str]:
    installed = environ.get("QKBOT_NONEBOT_IMAGE_PROFILE", "core")
    selected = environ.get("QKBOT_NONEBOT_PROFILE", installed)
    allowed = {installed, "core"}
    if installed == "full":
        allowed.update(PROFILES)
    elif installed == "parser-full":
        allowed.add("parser")
    if installed not in PROFILES or selected not in allowed:
        raise ValueError("requested profile is not provided by this image")
    result = [
        sys.executable,
        str(script),
        "--url", environ.get("QKBOT_NONEBOT_URL", "http://127.0.0.1:3215"),
        "--work-dir", environ.get("QKBOT_NONEBOT_WORK_DIR", "/data"),
        "--concurrency", environ.get("QKBOT_NONEBOT_CONCURRENCY", "8"),
        "--task-timeout", environ.get("QKBOT_NONEBOT_TASK_TIMEOUT", "120"),
        "--media-max-mib", environ.get("QKBOT_NONEBOT_MEDIA_MAX_MIB", "20"),
    ]
    for profile in PROFILES[selected]:
        result.extend(("--profile", profile))
    if config := environ.get("QKBOT_NONEBOT_PLUGIN_CONFIG"):
        result.extend(("--plugin-config", config))
    return [*result, *extra]


def prepare_preferences(work: Path) -> None:
    """Keep WITFF's existing file-based preferences on the dedicated volume."""
    work.mkdir(parents=True, exist_ok=True)
    path = work / "witff-prefs.json"
    try:
        with path.open("x", encoding="utf-8") as output:
            output.write("{}\n")
        path.chmod(0o600)
    except FileExistsError:
        pass


def main() -> int:
    try:
        os.environ["QKBOT_NONEBOT_TOKEN"] = bridge_token(os.environ)
        args = command(os.environ, sys.argv[1:], Path(__file__).with_name("run.py"))
        # The image contains a symlink from WITFF's package path to /data.
        # Other profiles may use the same volume later without losing settings.
        prepare_preferences(Path("/data"))
        os.execv(sys.executable, args)
    except (ValueError, OSError) as error:
        # OSError filenames may be operator-provided sensitive paths; don't echo.
        message = str(error) if isinstance(error, ValueError) else "container setup failed; check volume and secret permissions"
        print(f"NoneBot container: {message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
