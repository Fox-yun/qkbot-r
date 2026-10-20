"""Real Rust process + real run.py + local Milky HTTP/WS mock.

No real QQ account, model, administrator, production config, or persistent data.
The supplied Rust binary must already be built. Uses the current Python venv.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
from collections import deque
from contextlib import closing
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

import aiohttp
from aiohttp import web

ROOT = Path(__file__).resolve().parents[1]
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII="
)


def event(sequence: int, text: str) -> dict:
    stamp = int(time.time())
    return {
        "event_type": "message_receive", "self_id": 123, "time": stamp,
        "data": {
            "message_scene": "group", "peer_id": 456, "message_seq": sequence,
            "sender_id": 789, "time": stamp, "segments": [{"type": "text", "data": {"text": text}}],
            "group": {"group_id": 456, "group_name": "isolated-test", "member_count": 2,
                      "max_member_count": 200},
            "group_member": {
                "user_id": 789, "nickname": "tester", "sex": "unknown", "group_id": 456,
                "card": "", "title": "", "level": 1, "role": "member",
                "join_time": 1, "last_sent_time": stamp,
            },
        },
    }


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


class Probe:
    def __init__(self, binary: Path, work: Path):
        self.binary, self.work = binary, work
        self.bridge_port = free_port()
        self.bridge_token, self.milky_token = secrets.token_hex(32), secrets.token_hex(32)
        self.processes: list[asyncio.subprocess.Process] = []
        self.log_tasks: list[asyncio.Task] = []
        self.logs: deque[str] = deque(maxlen=120)
        self.socket_ready = asyncio.Event()
        self.websocket: web.WebSocketResponse | None = None
        self.sends: list[dict] = []
        self.changed = asyncio.Event()
        self.ack = 1000
        self.database = work / "probe.sqlite3"

    def authenticated(self, request: web.Request) -> None:
        if request.headers.get("Authorization") != f"Bearer {self.milky_token}":
            raise web.HTTPUnauthorized()
        assert not request.query, "Milky credentials must not appear in query parameters"

    async def events(self, request: web.Request) -> web.StreamResponse:
        self.authenticated(request)
        websocket = web.WebSocketResponse()
        await websocket.prepare(request)
        self.websocket = websocket
        self.socket_ready.set()
        async for message in websocket:
            assert message.type != aiohttp.WSMsgType.TEXT, "Rust must not write fabricated QQ events"
        return websocket

    async def api(self, request: web.Request) -> web.Response:
        self.authenticated(request)
        action = request.match_info["action"]
        body = await request.json()
        if action == "get_login_info":
            result = {"uin": 123, "nickname": "isolated bot"}
        elif action == "get_impl_info":
            result = {"impl_name": "qkbot-local-probe", "impl_version": "1", "milky_version": "1.2"}
        elif action == "get_message":
            assert body["message_scene"] == "group" and body["peer_id"] == 456
            result = {"message": event(body["message_seq"], "quoted fixture")["data"]}
        elif action == "send_group_message":
            assert body["group_id"] == 456
            assert "qkbot" not in body, "task metadata must be stripped before actual Milky"
            images = []
            for segment in body["message"]:
                if segment["type"] == "image":
                    uri = segment["data"]["uri"]
                    assert uri.startswith(f"http://127.0.0.1:{self.bridge_port}/media/")
                    async with aiohttp.ClientSession(trust_env=False) as client:
                        async with client.get(uri, allow_redirects=False) as response:
                            assert response.status == 200, "Milky must be able to fetch the capability"
                            assert response.headers["Content-Type"].startswith("image/png")
                            images.append(await response.read())
            self.ack += 1
            self.sends.append({"body": body, "images": images, "ack": self.ack})
            self.changed.set()
            result = {"message_seq": self.ack, "time": int(time.time())}
        else:
            raise AssertionError(f"unexpected external API {action}")
        return web.json_response({"status": "ok", "retcode": 0, "data": result})

    async def launch(self, *arguments: str, name: str, extra_env: dict | None = None) -> None:
        # No live credentials or proxy settings are inherited by either process.
        env = {
            key: value for key, value in os.environ.items()
            if not key.upper().startswith("QKBOT_")
            and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")
        }
        env.update(PYTHONUTF8="1", PYTHONUNBUFFERED="1")
        env.update(extra_env or {})
        options = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
        process = await asyncio.create_subprocess_exec(
            *arguments, cwd=self.work, env=env,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT, **options,
        )
        self.processes.append(process)

        async def collect() -> None:
            assert process.stdout is not None
            async for line in process.stdout:
                text = line.decode("utf-8", errors="replace").strip()
                for token in (self.bridge_token, self.milky_token):
                    text = text.replace(token, "[redacted]")
                self.logs.append(f"{name}: {text}")

        self.log_tasks.append(asyncio.create_task(collect()))

    def check_running(self) -> None:
        assert all(process.returncode is None for process in self.processes), "test process exited"

    async def wait_sends(self, count: int, timeout: float = 5) -> None:
        async with asyncio.timeout(timeout):
            while len(self.sends) < count:
                self.check_running()
                self.changed.clear()
                if len(self.sends) < count:
                    await self.changed.wait()

    async def close(self) -> None:
        # Core-only sidecar creates no browser/ffmpeg subprocess. Terminate only
        # these exact handles; never enumerate or kill unrelated processes.
        for process in reversed(self.processes):
            if process.returncode is None:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 8)
                except TimeoutError:
                    process.kill()
                    await process.wait()
        await asyncio.gather(*self.log_tasks, return_exceptions=True)

    async def run(self) -> None:
        app = web.Application()
        app.router.add_post("/api/{action}", self.api)
        app.router.add_get("/event", self.events)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        milky_port = site._server.sockets[0].getsockname()[1]
        while self.bridge_port == milky_port:
            self.bridge_port = free_port()
        config = self.work / "config.toml"
        # JSON string encoding is compatible with these simple TOML strings.
        config.write_text(f"""
[app]
database = {json.dumps(self.database.as_posix())}
log = "warn"
[web]
enabled = false
[qq_login]
enabled = false
[milky]
url = "http://127.0.0.1:{milky_port}"
access_token = "{self.milky_token}"
bot_id = 123
request_timeout_secs = 5
[plugins.ai]
enabled = false
[plugins.clock]
enabled = false
[plugins.reminder]
enabled = false
[plugins.native]
enabled = []
[nonebot]
enabled = true
listen = "127.0.0.1:{self.bridge_port}"
token = "{self.bridge_token}"
media_dir = {json.dumps((self.work / "media").as_posix())}
task_timeout_secs = 30
[[nonebot.plugins]]
id = "nb_echo"
module = "qkbot_echo"
name = "isolated echo"
description = "local integration probe"
enabled = true
commands = ["/nbtest"]
url_hosts = []
patterns = []
[limits]
send_cooldown_secs = 0
""", encoding="utf-8")
        try:
            await self.launch(str(self.binary), "--config", str(config), name="rust")
            await asyncio.wait_for(self.socket_ready.wait(), 15)
            async with aiohttp.ClientSession(
                trust_env=False, headers={"Authorization": f"Bearer {self.bridge_token}"}
            ) as client:
                async with asyncio.timeout(15):
                    while True:
                        self.check_running()
                        try:
                            async with client.get(
                                f"http://127.0.0.1:{self.bridge_port}/qkbot/catalog"
                            ) as response:
                                if response.status == 200:
                                    catalog = await response.json()
                                    assert catalog["plugins"][0]["module"] == "qkbot_echo"
                                    break
                        except aiohttp.ClientError:
                            pass
                        await asyncio.sleep(0.1)
            await self.launch(
                sys.executable, str(ROOT / "run.py"),
                "--url", f"http://127.0.0.1:{self.bridge_port}",
                "--work-dir", str(self.work / "sidecar"), name="python",
                extra_env={"QKBOT_NONEBOT_TOKEN": self.bridge_token},
            )
            assert self.websocket is not None
            # Public catalog is available before the WS subscriber. Harmless
            # tagged events prove end-to-end readiness without a test-only hook.
            ready_message = [{"type": "text", "data": {"text": "__ready__"}}]
            for index in range(12):
                await self.websocket.send_json(event(100 + index, "/nbtest text __ready__"))
                try:
                    async with asyncio.timeout(1):
                        while not any(send["body"]["message"] == ready_message for send in self.sends):
                            self.check_running()
                            self.changed.clear()
                            await self.changed.wait()
                    break
                except TimeoutError:
                    continue
            assert any(send["body"]["message"] == ready_message for send in self.sends), (
                "real Python sidecar did not consume Rust events"
            )
            baseline = len(self.sends)

            expected = (
                (201, "/nbtest text cross-process"),
                (202, "/nbtest reply quote-result"),
                (203, "/nbtest image"),
                (204, "/nbtest repeat identical"),
            )
            for sequence, text in expected:
                before = len(self.sends)
                await self.websocket.send_json(event(sequence, text))
                try:
                    await self.wait_sends(before + (2 if sequence == 204 else 1))
                except TimeoutError as error:
                    raise AssertionError(f"no complete response for event {sequence} ({text})") from error
            await asyncio.sleep(0.15)
            selected = [
                send for send in self.sends[baseline:] if send["body"]["message"] != ready_message
            ]
            assert len(selected) == 5, f"expected exactly five actual sends, got {len(selected)}"
            assert selected[0]["body"]["message"] == [
                {"type": "text", "data": {"text": "cross-process"}}
            ]
            assert selected[1]["body"]["message"] == [
                {"type": "reply", "data": {"message_seq": 202}},
                {"type": "text", "data": {"text": "quote-result"}},
            ]
            assert selected[2]["images"] == [PNG]
            repeat = [{"type": "text", "data": {"text": "identical"}}]
            assert selected[3]["body"]["message"] == selected[4]["body"]["message"] == repeat
            assert len({send["ack"] for send in selected}) == 5
            # Inspect the durable ledger read-only: each real ack must survive
            # the Rust/Python round trip and repeated text uses separate IDs.
            async with asyncio.timeout(5):
                while True:
                    with closing(sqlite3.connect(self.database.as_uri() + "?mode=ro", uri=True)) as database:
                        rows = database.execute(
                            "SELECT id,state,message_seq FROM outbox WHERE origin_plugin='nb_echo'"
                        ).fetchall()
                    confirmed = {row[2] for row in rows if row[1] == "confirmed"}
                    if all(send["ack"] in confirmed for send in selected):
                        break
                    await asyncio.sleep(0.05)
            assert len({row[0] for row in rows}) == len(rows)
            assert not (self.work / ".qkbot-admin" / "credentials.json").exists()
            print("PASS: real Rust + run.py text/reply/image/repeat; five confirmed acknowledgements")
            print("PASS: PNG capability fetched by mock Milky; duplicate text has distinct outbox IDs")
        except BaseException:
            print("mock sends: " + json.dumps(self.sends, ensure_ascii=False, default=lambda _: "<bytes>"),
                  file=sys.stderr)
            for line in list(self.logs)[-30:]:
                print(line, file=sys.stderr)
            raise
        finally:
            await self.close()
            await runner.cleanup()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bot-bin", type=Path, required=True)
    args = parser.parse_args()
    binary = args.bot_bin.resolve()
    if not binary.is_file():
        parser.error("Rust binary must already exist; this probe does not build it")
    with tempfile.TemporaryDirectory(prefix="qkbot-rust-nonebot-") as directory:
        copied = Path(directory) / binary.name
        shutil.copyfile(binary, copied)
        asyncio.run(Probe(copied, Path(directory)).run())


if __name__ == "__main__":
    main()
