"""Run inside the core image: real entrypoint + isolated loopback bridge mock."""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import secrets
import sys
import tempfile
import time

from aiohttp import web


async def probe() -> None:
    assert os.getuid() == 10001, "the image must run as its non-root user"
    token = secrets.token_hex(32)
    generation = "container-probe-" + secrets.token_hex(8)
    connected = asyncio.Event()
    sent = asyncio.Event()
    sockets: list[web.WebSocketResponse] = []
    writes: list[dict] = []
    requests: list[str] = []

    def authenticate(request: web.Request) -> None:
        if request.headers.get("Authorization") != f"Bearer {token}":
            raise web.HTTPUnauthorized()
        assert not request.query, "credentials must stay out of the URL"

    async def catalog(request: web.Request) -> web.Response:
        authenticate(request)
        return web.json_response({"plugins": [
            {"id": "nb_echo", "module": "qkbot_echo", "enabled": True}
        ]})

    async def api(request: web.Request) -> web.Response:
        authenticate(request)
        action = request.match_info["action"]
        requests.append(action)
        data = await request.json()
        if action == "get_login_info":
            result = {"uin": 123, "nickname": "container-probe"}
        elif action == "send_group_message":
            writes.append(data)
            result = {"message_seq": 1000 + len(writes), "time": int(time.time())}
            sent.set()
        else:
            raise web.HTTPBadRequest()
        return web.json_response({"status": "ok", "retcode": 0, "data": result})

    async def events(request: web.Request) -> web.WebSocketResponse:
        authenticate(request)
        socket = web.WebSocketResponse()
        await socket.prepare(request)
        sockets.append(socket)
        connected.set()
        async for _ in socket:
            pass
        return socket

    app = web.Application()
    app.router.add_get("/qkbot/catalog", catalog)
    app.router.add_post("/api/{action}", api)
    app.router.add_get("/event", events)
    runner = web.AppRunner(app)
    await runner.setup()
    server = web.TCPSite(runner, "127.0.0.1", 0)
    await server.start()
    port = server._server.sockets[0].getsockname()[1]
    process: asyncio.subprocess.Process | None = None
    output_task: asyncio.Task | None = None
    try:
        with tempfile.TemporaryDirectory(prefix="core-probe-", dir="/data") as temporary:
            work = Path(temporary)
            secret = work / "token"
            secret.write_text(token + "\n", encoding="ascii")
            secret.chmod(0o600)
            env = os.environ.copy()
            env.pop("QKBOT_NONEBOT_TOKEN", None)
            env.update(
                QKBOT_NONEBOT_TOKEN_FILE=str(secret),
                QKBOT_NONEBOT_URL=f"http://127.0.0.1:{port}",
                QKBOT_NONEBOT_WORK_DIR=str(work / "work"),
                QKBOT_NONEBOT_PROFILE="core",
            )
            process = await asyncio.create_subprocess_exec(
                sys.executable, "/opt/qkbot-nonebot/container-entrypoint.py",
                env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
            )
            output_task = asyncio.create_task(process.communicate())
            await asyncio.wait_for(connected.wait(), 20)
            stamp = int(time.time())
            await sockets[0].send_json({
                "event_type": "message_receive", "self_id": 123, "time": stamp,
                "qkbot": {"task_id": "core-container-task", "plugin_id": "nb_echo", "generation": generation},
                "data": {
                    "message_scene": "group", "peer_id": 456, "message_seq": 77,
                    "sender_id": 789, "time": stamp,
                    "segments": [{"type": "text", "data": {"text": "/nbtest text container-ok"}}],
                    "group": {"group_id": 456, "group_name": "probe", "member_count": 2, "max_member_count": 200},
                    "group_member": {
                        "user_id": 789, "nickname": "probe", "sex": "unknown", "group_id": 456,
                        "card": "", "title": "", "level": 1, "role": "member",
                        "join_time": 1, "last_sent_time": stamp,
                    },
                },
            })
            await asyncio.wait_for(sent.wait(), 10)
            assert len(writes) == 1
            message = writes[0]
            assert message["group_id"] == 456
            assert message["message"] == [{"type": "text", "data": {"text": "container-ok"}}]
            assert message["qkbot"]["task_id"] == "core-container-task"
            assert message["qkbot"]["generation"] == generation
            assert message["qkbot"]["plugin_id"] == "nb_echo"
            assert len(message["qkbot"]["operation_id"]) == 32
            assert (work / "work/cache").is_dir()
            assert Path("/data/witff-prefs.json").read_text(encoding="utf-8") == "{}\n"
            process.terminate()
            output, _ = await asyncio.wait_for(asyncio.shield(output_task), 20)
            assert process.returncode == 0, f"unclean stop: {process.returncode}"
            assert token.encode() not in output, "credential leaked to process output"
            print(json.dumps({
                "result": "ok", "uid": os.getuid(), "python": sys.version.split()[0],
                "profile": "core", "authenticated_websocket": True,
                "confirmed_sends": len(writes), "exit_code": process.returncode,
                "secret_file": True, "persistent_workdir_writable": True,
            }))
    finally:
        if process is not None and process.returncode is None:
            process.kill()
            await process.wait()
        if output_task is not None:
            await output_task
        await runner.cleanup()


if __name__ == "__main__":
    asyncio.run(probe())
