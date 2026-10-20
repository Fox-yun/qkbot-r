from __future__ import annotations

import asyncio
import base64
import copy
import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

import nonebot
from aiohttp import web
from nonebot.adapters.milky import Bot
from nonebot.adapters.milky.config import ClientInfo
from nonebot.adapters.milky.exception import NetworkError
from nonebot.matcher import matchers
from nonebot.rule import Rule

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent / "fixtures"))

from qkbot_bridge.adapter import Adapter, Lease, SESSION_MARKER, current_lease
from qkbot_bridge.transport import BODY_LIMIT, BridgeError, Settings, Transport, detect_media
from qkbot_bridge.environment import redact_log

_prior_cwd = Path.cwd()
os.chdir(ROOT)
nonebot.init(
    _env_file=(ROOT / "__test_no_dotenv__",),
    driver="~aiohttp",
    log_level="CRITICAL",
    milky_clients=[],
    command_start={"/"},
    session_expire_timeout=120,
)
os.chdir(_prior_cwd)
assert nonebot.load_plugin("qkbot_bridge.plugin")
assert nonebot.load_plugin("qkbot_echo")
assert nonebot.load_plugin("qkbot_test_plugin")

from qkbot_echo import PNG
from qkbot_test_plugin import BACKGROUND_RESULTS
import qkbot_test_plugin


def event(text: str = "/nbtest text hello", task: str = "task-one", plugin: str = "nb_echo") -> dict:
    return {
        "event_type": "message_receive",
        "time": 1,
        "self_id": 123,
        "qkbot": {"task_id": task, "plugin_id": plugin, "generation": "7"},
        "data": {
            "message_scene": "group",
            "peer_id": 456,
            "message_seq": 91,
            "sender_id": 789,
            "time": 1,
            "segments": [{"type": "text", "data": {"text": text}}],
            "group": {"group_id": 456, "group_name": "test", "member_count": 2, "max_member_count": 200},
            "group_member": {
                "user_id": 789, "nickname": "tester", "sex": "unknown", "group_id": 456,
                "card": "", "title": "", "level": 1, "role": "member",
                "join_time": 1, "last_sent_time": 1,
            },
        },
    }


class BridgeTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="qkbot-nonebot-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.requests: list[tuple[str, dict]] = []
        self.uploads: list[bytes] = []
        self.media_stream_size: int | None = None
        self.streamed_bytes = 0
        self.leases: list[dict] = []
        self.websocket_ready = asyncio.Event()
        self.websocket: web.WebSocketResponse | None = None
        self.ws_query: str | None = None
        self.response_mode = "normal"
        self.bindings = {"qkbot_echo": "nb_echo", "qkbot_test_plugin": "nb_test"}
        self.token = "test-sidecar-token-" + "x" * 32
        app = web.Application()
        app.router.add_get("/qkbot/catalog", self.catalog)
        app.router.add_post("/api/{action}", self.api)
        app.router.add_post("/qkbot/media", self.media)
        app.router.add_post("/qkbot/lease", self.lease)
        app.router.add_get("/event", self.events)
        runner = web.AppRunner(app)
        await runner.setup()
        self.addAsyncCleanup(runner.cleanup)
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        self.settings = Settings(
            self.url, self.token,
            allowed_modules=frozenset(self.bindings),
            media_roots=(self.root,),
        )
        self.adapter = Adapter(nonebot.get_driver(), settings=self.settings, bindings=self.bindings)
        self.bot = Bot(self.adapter, "123", ClientInfo())
        self.addAsyncCleanup(self.adapter._stop_bridge)

    def authenticated(self, request: web.Request) -> None:
        self.assertEqual(request.headers.get("Authorization"), f"Bearer {self.token}")
        self.assertNotIn(self.token, str(request.rel_url))

    async def catalog(self, request: web.Request) -> web.Response:
        self.authenticated(request)
        return web.json_response({"plugins": [
            {"module": module, "id": plugin_id, "enabled": True}
            for module, plugin_id in self.bindings.items()
        ]})

    async def api(self, request: web.Request) -> web.Response:
        self.authenticated(request)
        data = await request.json()
        action = request.match_info["action"]
        self.requests.append((action, data))
        if self.response_mode == "large":
            return web.Response(body=b"x" * (BODY_LIMIT + 1))
        if self.response_mode == "invalid":
            return web.Response(text="not JSON")
        if self.response_mode == "redirect":
            return web.Response(status=302, headers={"Location": self.url + "/api/leak"})
        if self.response_mode == "error":
            return web.Response(status=500, text="private upstream error " + self.token)
        if action == "get_login_info":
            result = {"uin": 123, "nickname": "test"}
        elif action == "get_message":
            result = {"message": event()["data"]}
        else:
            result = {"message_seq": len(self.requests) + 100, "time": 1}
        return web.json_response({"status": "ok", "retcode": 0, "data": result})

    async def media(self, request: web.Request) -> web.Response:
        self.authenticated(request)
        self.assertEqual(request.headers["X-Qkbot-Task"], "task-image")
        self.assertEqual(request.headers["X-Qkbot-Plugin"], "nb_echo")
        self.assertEqual(request.headers["X-Qkbot-Generation"], "7")
        self.assertEqual(request.headers["Content-Type"], "image/png")
        if self.media_stream_size is None:
            self.uploads.append(await request.read())
        else:
            async for chunk in request.content.iter_chunked(65536):
                self.streamed_bytes += len(chunk)
            self.assertEqual(self.streamed_bytes, self.media_stream_size)
            # The upload gets its task-specific timeout, not the shorter
            # ordinary API timeout selected by this regression.
            await asyncio.sleep(1.1)
        return web.json_response({"url": self.url + "/media/capability", "media_id": "capability"})

    async def lease(self, request: web.Request) -> web.Response:
        self.authenticated(request)
        self.leases.append(await request.json())
        return web.json_response({"ok": True})

    async def events(self, request: web.Request) -> web.WebSocketResponse:
        self.authenticated(request)
        self.ws_query = request.query_string
        socket = web.WebSocketResponse()
        await socket.prepare(request)
        self.websocket = socket
        self.websocket_ready.set()
        async for _ in socket:
            pass
        return socket

    async def process(self, raw: dict) -> None:
        parsed = self.adapter.parse_event(raw, 123)
        self.assertIsNotNone(parsed)
        await self.adapter.process_event(self.bot, *parsed)

    def sends(self) -> list[dict]:
        return [data for action, data in self.requests if action.startswith("send_")]

    def pending(self, adapter: Adapter | None = None) -> list:
        instance = (adapter or self.adapter)._instance_id
        return [
            matcher for candidates in matchers.values() for matcher in candidates
            if matcher.temp and matcher._default_state.get(SESSION_MARKER, (None,))[0] == instance
        ]

    async def test_official_model_and_matcher_send_identity(self) -> None:
        raw = event()
        parsed = self.adapter.parse_event(raw, 123)
        self.assertEqual(parsed[0].qkbot, raw["qkbot"])
        await self.adapter.process_event(self.bot, *parsed)
        sends = self.sends()
        self.assertEqual(len(sends), 1)
        self.assertEqual(sends[0]["group_id"], 456)
        self.assertEqual(sends[0]["message"], [{"type": "text", "data": {"text": "hello"}}])
        self.assertEqual(sends[0]["qkbot"]["plugin_id"], "nb_echo")
        self.assertEqual(sends[0]["qkbot"]["generation"], "7")
        self.assertEqual(sends[0]["qkbot"]["task_id"], "task-one")
        self.assertEqual(len(sends[0]["qkbot"]["operation_id"]), 32)
        self.assertIsNone(self.adapter.parse_event(raw, 123), "a repeated event is not executed twice")

    async def test_equal_sends_are_distinct_operations(self) -> None:
        await self.process(event("/nbtest repeat same"))
        first, second = self.sends()
        self.assertEqual(first["message"], second["message"])
        self.assertNotEqual(first["qkbot"]["operation_id"], second["qkbot"]["operation_id"])

    async def test_missing_wrong_owner_and_foreign_bot_are_rejected(self) -> None:
        raw = event()
        raw.pop("qkbot")
        self.assertIsNone(self.adapter.parse_event(raw, 123))
        self.assertIsNone(self.adapter.parse_event(event(), 999))
        await self.process(event(plugin="nb_test"))
        self.assertEqual(self.sends(), [])
        with self.assertRaises(NetworkError):
            await self.bot.send_group_message(group_id=456, message="unbound")
        self.assertEqual(self.sends(), [])

    async def test_background_send_is_revoked_when_event_finishes(self) -> None:
        BACKGROUND_RESULTS.clear()
        qkbot_test_plugin.BACKGROUND_GATE = asyncio.Event()
        await self.process(event("/background", plugin="nb_test"))
        qkbot_test_plugin.BACKGROUND_GATE.set()
        async with asyncio.timeout(1):
            while not BACKGROUND_RESULTS:
                await asyncio.sleep(0)
        self.assertEqual(len(BACKGROUND_RESULTS), 1)
        self.assertIsInstance(BACKGROUND_RESULTS[0], NetworkError)
        self.assertEqual(self.sends(), [])

    async def test_official_got_requests_bounded_scope_lease(self) -> None:
        await self.process(event("/conversation", task="conversation-one", plugin="nb_test"))
        self.assertEqual(self.leases, [{
            "task_id": "conversation-one", "plugin_id": "nb_test", "generation": "7", "seconds": 120
        }])
        await self.process(event("my answer", task="conversation-two", plugin="nb_test"))
        self.assertEqual(self.sends()[-1]["message"], [{"type": "text", "data": {"text": "done"}}])
        self.assertEqual(len(self.leases), 1)

    async def test_pending_conversation_is_bound_to_account_scope_actor_and_generation(self) -> None:
        await self.process(event("/conversation", task="original", plugin="nb_test"))
        original = self.pending()[0]
        for change in ("bot", "group", "actor", "generation"):
            raw = event("/conversation", task=change, plugin="nb_test")
            if change == "bot":
                raw["self_id"] = 999
            elif change == "group":
                raw["data"]["peer_id"] = 457
                raw["data"]["group"]["group_id"] = 457
                raw["data"]["group_member"]["group_id"] = 457
            elif change == "actor":
                raw["data"]["sender_id"] = 790
                raw["data"]["group_member"]["user_id"] = 790
            else:
                raw["qkbot"]["generation"] = "new-generation"
            parsed = self.adapter.parse_event(raw, raw["self_id"])
            self.assertIsNotNone(parsed)
            await self.adapter.process_event(Bot(self.adapter, str(raw["self_id"]), ClientInfo()), *parsed)
            self.assertEqual(self.sends()[-1]["message"], [{"type": "text", "data": {"text": "next?"}}])
            self.assertIn(original, self.pending(), "foreign events must not consume the original wait")
        await self.process(event("answer", task="answer-original", plugin="nb_test"))
        self.assertEqual(self.sends()[-1]["message"], [{"type": "text", "data": {"text": "done"}}])
        self.assertNotIn(original, self.pending())

    async def test_preprocessor_checks_old_binding_before_replacing_state(self) -> None:
        await self.process(event("/conversation", task="old-state", plugin="nb_test"))
        # Defense remains effective even if another trusted hook replaces Rule.
        self.pending()[0].rule = Rule()
        raw = event("/conversation", task="new-state", plugin="nb_test")
        raw["qkbot"]["generation"] = "replacement-generation"
        await self.process(raw)
        self.assertEqual(
            [message["message"] for message in self.sends()],
            [[{"type": "text", "data": {"text": "next?"}}]] * 2,
        )

    async def test_disconnect_cleanup_does_not_delete_another_bridge_wait(self) -> None:
        await self.process(event("/conversation", task="first-bridge", plugin="nb_test"))
        other = Adapter(nonebot.get_driver(), settings=self.settings, bindings=self.bindings)
        self.addAsyncCleanup(other._stop_bridge)
        raw = event("/conversation", task="second-bridge", plugin="nb_test")
        await other.process_event(Bot(other, "123", ClientInfo()), *other.parse_event(raw, 123))
        self.assertEqual(len(self.pending()), 1)
        self.assertEqual(len(self.pending(other)), 1)
        await self.adapter._stop_bridge()
        self.assertEqual(self.pending(), [])
        self.assertEqual(len(self.pending(other)), 1)
        answer = event("answer", task="second-answer", plugin="nb_test")
        await other.process_event(Bot(other, "123", ClientInfo()), *other.parse_event(answer, 123))
        self.assertEqual(self.sends()[-1]["message"], [{"type": "text", "data": {"text": "done"}}])

    async def test_real_websocket_disconnect_discards_wait_before_reconnect(self) -> None:
        task = asyncio.create_task(self.adapter._connection_loop())
        self.adapter.tasks.add(task)
        task.add_done_callback(self.adapter.tasks.discard)
        await asyncio.wait_for(self.websocket_ready.wait(), 3)
        old_socket = self.websocket
        await old_socket.send_json(event("/conversation", task="old-connection", plugin="nb_test"))
        async with asyncio.timeout(3):
            while not self.leases:
                await asyncio.sleep(0.01)
        self.assertEqual(len(self.pending()), 1)
        await old_socket.close()
        async with asyncio.timeout(3):
            while self.pending():
                await asyncio.sleep(0.01)
        async with asyncio.timeout(4):
            while self.websocket is old_socket:
                await asyncio.sleep(0.01)
        fresh = event("/conversation", task="fresh-connection", plugin="nb_test")
        fresh["qkbot"]["generation"] = "fresh-generation"
        await self.websocket.send_json(fresh)
        async with asyncio.timeout(3):
            while len(self.leases) < 2:
                await asyncio.sleep(0.01)
        self.assertEqual(self.sends()[-1]["message"], [{"type": "text", "data": {"text": "next?"}}])
        answer = event("answer", task="fresh-answer", plugin="nb_test")
        answer["qkbot"]["generation"] = "fresh-generation"
        await self.websocket.send_json(answer)
        async with asyncio.timeout(3):
            while len(self.sends()) < 3:
                await asyncio.sleep(0.01)
        self.assertEqual(self.sends()[-1]["message"], [{"type": "text", "data": {"text": "done"}}])

    async def test_reply_and_base64_image_preserve_official_shapes(self) -> None:
        await self.process(event("/nbtest reply quoted", task="reply"))
        self.assertEqual(self.sends()[0]["message"][0], {"type": "reply", "data": {"message_seq": 91}})
        await self.process(event("/nbtest image", task="task-image"))
        self.assertEqual(self.uploads, [PNG])
        image = self.sends()[1]["message"][0]
        self.assertEqual(image["type"], "image")
        self.assertEqual(image["data"]["uri"], self.url + "/media/capability")
        self.assertNotIn("base64://", json.dumps(self.sends()))

    async def test_safe_files_stream_and_outside_roots_are_rejected(self) -> None:
        path = self.root / "tiny.png"
        path.write_bytes(PNG)
        identity = event(task="task-image")["qkbot"]
        await self.adapter.transport.upload(path.as_uri(), identity)
        self.assertEqual(self.uploads, [PNG])
        percent_name = self.root / "literal%20 name.png"
        percent_name.write_bytes(PNG)
        await self.adapter.transport.upload(percent_name.as_uri(), identity)
        self.assertEqual(self.uploads, [PNG, PNG])
        with self.assertRaises(BridgeError):
            await self.adapter.transport.upload(Path(__file__).resolve().as_uri(), identity)
        with self.assertRaises(BridgeError):
            await self.adapter.transport.upload("file://foreign-host/share/image.png", identity)
        with self.assertRaises(BridgeError):
            await self.adapter.transport.upload("base64://not base64", identity)
        with self.assertRaises(BridgeError):
            await self.adapter.transport.upload("base64://" + base64.b64encode(b"not media").decode(), identity)
        self.assertEqual(len(self.uploads), 2)

    async def test_no_redirect_no_retry_and_bounded_json(self) -> None:
        for mode in ("redirect", "invalid", "large", "error"):
            self.response_mode = mode
            before = len(self.requests)
            with self.assertRaises(NetworkError) as caught:
                await self.adapter.call_http(ClientInfo(), "get_login_info")
            self.assertNotIn(self.token, str(caught.exception))
            self.assertEqual(len(self.requests), before + 1)
        self.assertTrue(all(action != "leak" for action, _ in self.requests))

    async def test_large_file_streams_with_upload_specific_timeout(self) -> None:
        size = 51 * 1024 * 1024
        path = self.root / "large.png"
        with path.open("wb") as handle:
            handle.write(PNG)
            handle.seek(size - 1)
            handle.write(b"\0")
        self.media_stream_size = size
        transport = Transport(Settings(
            self.url, self.token, media_roots=(self.root,), media_max_bytes=64 * 1024 * 1024,
            http_timeout=1, task_timeout=60,
        ))
        self.addAsyncCleanup(transport.close)
        url = await transport.upload(path.as_uri(), event(task="task-image")["qkbot"])
        self.assertEqual(url, self.url + "/media/capability")
        self.assertEqual(self.streamed_bytes, size)
        self.assertEqual(self.uploads, [], "server must not collect the large body in memory")

    async def test_real_websocket_dispatch_has_bearer_and_no_query_token(self) -> None:
        task = asyncio.create_task(self.adapter._connection_loop())
        self.adapter.tasks.add(task)
        task.add_done_callback(self.adapter.tasks.discard)
        await asyncio.wait_for(self.websocket_ready.wait(), 3)
        await self.websocket.send_json(event())
        async with asyncio.timeout(3):
            while not self.sends():
                await asyncio.sleep(0.01)
        self.assertEqual(self.ws_query, "")
        self.assertEqual(len(self.sends()), 1)

    async def test_catalog_does_not_load_unapproved_module(self) -> None:
        self.bindings["not_allowed"] = "nb_other"
        with self.assertRaises(BridgeError):
            await self.adapter.transport.catalog()

    async def test_runtime_generation_is_an_opaque_string(self) -> None:
        raw = event(task="opaque-generation")
        raw["qkbot"]["generation"] = "da1e5f9d-6371-4fb0-8b10-021e3ec286ab"
        parsed = self.adapter.parse_event(raw, 123)
        self.assertIsNotNone(parsed)
        self.assertEqual(parsed[1].identity["generation"], raw["qkbot"]["generation"])
        for invalid in (7, True, "", "line\nbreak"):
            raw["qkbot"]["generation"] = invalid
            self.assertIsNone(self.adapter.parse_event(raw, 123))

    async def test_reads_have_task_identity_and_expired_tasks_are_rejected(self) -> None:
        lease = Lease(event()["qkbot"], time.monotonic() + 10, self.adapter._instance_id)
        marker = current_lease.set(lease)
        try:
            await self.bot.get_message(message_scene="group", peer_id=456, message_seq=91)
            self.assertEqual(self.requests[-1][1]["qkbot"]["task_id"], "task-one")
            lease.active = False
            with self.assertRaises(NetworkError):
                await self.bot.get_message(message_scene="group", peer_id=456, message_seq=91)
        finally:
            current_lease.reset(marker)
        self.assertEqual(len(self.requests), 1)


class SettingsTests(unittest.TestCase):
    def test_media_detection_preserves_audio_formats(self) -> None:
        for prefix, expected in (
            (b"ID3" + b"\0" * 10, "audio/mpeg"),
            (b"\xff\xf1" + b"\0" * 10, "audio/aac"),
            (b"\xff\xf9" + b"\0" * 10, "audio/aac"),
            (b"OggS" + b"\0" * 10, "audio/ogg"),
            (b"fLaC" + b"\0" * 10, "audio/flac"),
        ):
            self.assertEqual(detect_media(prefix), expected)
        with self.assertRaises(BridgeError):
            detect_media(b"<!DOCTYPE html>")

    def test_plugin_http_errors_cannot_print_music_keys_or_bridge_token(self) -> None:
        original = "failed https://music.invalid/?uin=123&skey=top-secret%2Fvalue&n=1 auth bridge-secret"
        redacted = redact_log(original, ("bridge-secret", "top-secret/value"))
        self.assertNotIn("top-secret", redacted)
        self.assertNotIn("bridge-secret", redacted)
        self.assertIn("&n=1", redacted)

    def test_only_fixed_loopback_origin_is_accepted(self) -> None:
        token = "a" * 32
        for url in (
            "https://127.0.0.1:3215", "http://localhost:3215", "http://example.org:3215",
            "http://user@127.0.0.1:3215", "http://127.0.0.1:3215/?token=x",
        ):
            with self.assertRaises(BridgeError):
                Settings(url, token)


if __name__ == "__main__":
    unittest.main()
