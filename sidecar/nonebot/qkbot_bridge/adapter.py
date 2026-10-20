from __future__ import annotations

import asyncio
import contextlib
import copy
import json
import time
import uuid
from collections import OrderedDict
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Any

import aiohttp
from nonebot.adapters.milky import Adapter as MilkyAdapter
from nonebot.adapters.milky import Bot
from nonebot.adapters.milky.config import ClientInfo
from nonebot.adapters.milky.event import EVENT_CLASSES, Event, MessageEvent
from nonebot.adapters.milky.exception import NetworkError
from nonebot.adapters.milky.model import ModelBase
from nonebot.adapters.milky.utils import clean_params, handle_api_result
from nonebot.compat import type_validate_python
from nonebot.internal.matcher import current_matcher
from nonebot.log import logger
from nonebot.utils import DataclassEncoder

from .transport import BODY_LIMIT, BridgeError, Settings, Transport

READ_APIS = frozenset(
    {
        "get_message",
        "get_group_info",
        "get_group_member_info",
        "get_group_member_list",
        "get_friend_info",
        "get_resource_temp_url",
    }
)
BOOTSTRAP_APIS = frozenset({"get_login_info", "get_impl_info"})
SEND_APIS = frozenset({"send_group_message", "send_private_message"})
WRITE_APIS = SEND_APIS | frozenset({
    "send_group_message_reaction", "upload_group_file", "upload_private_file"
})


@dataclass
class Lease:
    identity: dict[str, Any]
    deadline: float
    adapter_id: str
    active: bool = True
    event: Event | None = None


current_lease: ContextVar[Lease] = ContextVar("qkbot_event_lease")
SESSION_MARKER = "_qkbot_session_binding"


def module_owner(matcher: Any, bindings: dict[str, str]) -> str | None:
    plugin = matcher.plugin
    while plugin is not None:
        if plugin.module_name in bindings:
            return bindings[plugin.module_name]
        plugin = plugin.parent_plugin
    return None


class Adapter(MilkyAdapter):
    """Official models/Bot/API methods, with only the controlled transport replaced."""

    def __init__(self, driver: Any, *, settings: Settings, bindings: dict[str, str]):
        self.settings = settings
        self.bindings = dict(bindings)
        self.transport = Transport(settings)
        self._event_tasks: set[asyncio.Task] = set()
        self._seen: OrderedDict[str, None] = OrderedDict()
        self._slots = asyncio.Semaphore(settings.concurrency)
        self.onebot_modules: set[str] = set()
        self.onebot_adapter: Any = None
        self._instance_id = uuid.uuid4().hex
        super().__init__(driver)

    def _setup(self) -> None:
        # No reverse webhook and no upstream Milky address or credentials.
        self.on_ready(self._start_bridge)
        self.driver.on_shutdown(self._stop_bridge)

    async def _start_bridge(self) -> None:
        task = asyncio.create_task(self._connection_loop(), name="qkbot-bridge")
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard)

    async def _stop_bridge(self) -> None:
        tasks = list(self.tasks | self._event_tasks)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        self.clear_pending_matchers()
        await self.transport.close()

    def session_binding(self) -> tuple:
        """Trusted physical session and connection generation, never matcher input."""
        try:
            lease = current_lease.get()
        except LookupError as error:
            raise BridgeError("no active event session") from error
        event = lease.event
        if (
            lease.adapter_id != self._instance_id
            or not lease.active or time.monotonic() >= lease.deadline
            or not isinstance(event, MessageEvent)
        ):
            raise BridgeError("event session has expired")
        return (
            self._instance_id, event.self_id, event.data.message_scene,
            event.data.peer_id, event.data.sender_id,
            lease.identity["plugin_id"], lease.identity["generation"],
        )

    def clear_pending_matchers(self) -> None:
        from nonebot.matcher import matchers

        # Only state created under this concrete bridge instance is ours.
        for candidates in list(matchers.values()):
            for candidate in list(candidates):
                binding = candidate._default_state.get(SESSION_MARKER)
                if (
                    candidate.temp and isinstance(binding, tuple)
                    and binding and binding[0] == self._instance_id
                ):
                    with contextlib.suppress(ValueError):
                        candidate.destroy()

    def identity(self, action: str, *, matcher_required: bool = False) -> dict[str, Any]:
        try:
            lease = current_lease.get()
        except LookupError as error:
            raise BridgeError("no active event identity") from error
        if lease.adapter_id != self._instance_id or not lease.active or time.monotonic() >= lease.deadline:
            raise BridgeError("event identity has expired")
        matcher = current_matcher.get(None)
        if matcher is None:
            if matcher_required or action not in READ_APIS:
                raise BridgeError("operation requires an active plugin matcher")
        elif module_owner(matcher, self.bindings) != lease.identity["plugin_id"]:
            raise BridgeError("matcher does not own this event")
        return dict(lease.identity)

    async def call_http(
        self, info: ClientInfo, action: str, params: dict | None = None
    ) -> dict:
        del info  # The official Bot's destination is intentionally not authoritative.
        if action not in BOOTSTRAP_APIS | READ_APIS | WRITE_APIS:
            raise NetworkError("API is not enabled by the qkbot bridge")
        try:
            data = clean_params(params or {})
            if "qkbot" in data:
                raise BridgeError("plugin-supplied identity is not accepted")
            data = {key: value.dict_() if isinstance(value, ModelBase) else value for key, value in data.items()}
            data = json.loads(json.dumps(data, cls=DataclassEncoder))
            if action not in BOOTSTRAP_APIS:
                identity = self.identity(action, matcher_required=action in WRITE_APIS)
                identity["operation_id"] = uuid.uuid4().hex
                if action in WRITE_APIS:
                    data = await self._prepare_media(data, identity)
                    self.identity(action, matcher_required=True)
                data["qkbot"] = identity
            response = await self.transport.request("POST", f"/api/{action}", json=data)
            if not isinstance(response, dict):
                raise BridgeError("invalid Milky response")
            return handle_api_result(response)
        except BridgeError as error:
            raise NetworkError(str(error)) from error

    async def _prepare_media(self, value: Any, identity: dict[str, Any], depth: int = 0) -> Any:
        if depth > 20:
            raise BridgeError("message nesting is too deep")
        if isinstance(value, list):
            if len(value) > 512:
                raise BridgeError("too many message elements")
            return [await self._prepare_media(item, identity, depth + 1) for item in value]
        if isinstance(value, dict):
            result = {}
            for key, child in value.items():
                if key in ("uri", "thumb_uri", "thumbnail_uri", "file_uri") and isinstance(child, str):
                    if child.startswith(("file://", "base64://")):
                        self.identity("send_group_message", matcher_required=True)
                        child = await self.transport.upload(child, identity)
                    elif not child.startswith(("https://", "http://")):
                        raise BridgeError("unsupported outgoing resource URI")
                    result[key] = child
                else:
                    result[key] = await self._prepare_media(child, identity, depth + 1)
            return result
        return value

    async def extend_conversation(self, seconds: int) -> None:
        identity = self.identity("lease", matcher_required=True)
        if not 1 <= seconds <= 120:
            raise BridgeError("conversation lease must be 1..120 seconds")
        await self.transport.request("POST", "/qkbot/lease", json={**identity, "seconds": seconds})

    def parse_event(self, raw: Any, bot_id: int) -> tuple[Event, Lease] | None:
        if not isinstance(raw, dict):
            return None
        identity = raw.get("qkbot")
        if not isinstance(identity, dict):
            return None
        task_id, plugin_id, generation = (
            identity.get("task_id"), identity.get("plugin_id"), identity.get("generation")
        )
        if (
            not isinstance(task_id, str) or not 1 <= len(task_id) <= 128
            or not isinstance(plugin_id, str) or plugin_id not in self.bindings.values()
            or not isinstance(generation, str) or not 1 <= len(generation) <= 128
            or any(not (char.isascii() and (char.isalnum() or char in "-_")) for char in generation)
            or raw.get("self_id") != bot_id
        ):
            return None
        key = f"{generation}:{plugin_id}:{task_id}"
        if key in self._seen:
            return None
        kind = raw.get("event_type")
        event_class = EVENT_CLASSES.get(kind)
        if event_class is None:
            return None
        try:
            # Same official model and conversion, without logging raw user messages on errors.
            event = type_validate_python(event_class, copy.deepcopy(raw))
            if isinstance(event, MessageEvent):
                event = event.convert()
        except (TypeError, ValueError):
            return None
        self._seen[key] = None
        if len(self._seen) > 2048:
            self._seen.popitem(last=False)
        return event, Lease(
            {"task_id": task_id, "plugin_id": plugin_id, "generation": generation},
            time.monotonic() + self.settings.task_timeout,
            self._instance_id,
        )

    async def process_event(self, bot: Bot, event: Event, lease: Lease) -> None:
        token = current_lease.set(lease)
        lease.event = event
        try:
            async with asyncio.timeout(self.settings.task_timeout):
                owner_module = next((module for module, owner in self.bindings.items()
                                     if owner == lease.identity["plugin_id"]), None)
                if owner_module in self.onebot_modules:
                    from .onebot import Adapter as OneBotAdapter, Bot as OneBot, event_for
                    if not isinstance(event, MessageEvent):
                        raise BridgeError("OneBot facade currently supports message events")
                    if self.onebot_adapter is None:
                        self.onebot_adapter = OneBotAdapter(self)
                    facade = self.onebot_adapter.bots.get(bot.self_id)
                    if facade is None:
                        facade = OneBot(self.onebot_adapter, bot.self_id)
                        self.onebot_adapter.bots[bot.self_id] = facade
                    await facade.handle_event(event_for(event))
                else:
                    await bot.handle_event(event)
        except TimeoutError:
            logger.warning("qkbot plugin event exceeded its time limit")
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.warning("qkbot plugin event failed")
        finally:
            lease.active = False
            current_lease.reset(token)

    async def _bounded_event(self, bot: Bot, event: Event, lease: Lease) -> None:
        try:
            await self.process_event(bot, event, lease)
        finally:
            self._slots.release()

    async def _connection_loop(self) -> None:
        delay = 1
        while True:
            bot: Bot | None = None
            try:
                # A changed catalog cannot silently give an already-loaded module a new identity.
                if await self.transport.catalog() != self.bindings:
                    raise BridgeError("catalog changed; restart the sidecar to reload plugins")
                info = await self.call_http(ClientInfo(), "get_login_info")
                if not isinstance(info, dict) or not isinstance(info.get("uin"), int) or info["uin"] <= 0:
                    raise BridgeError("QQ is not logged in")
                client = await self.transport.session()
                websocket_url = self.settings.url.replace("http://", "ws://", 1).rstrip("/") + "/event"
                async with client.ws_connect(
                    websocket_url, max_msg_size=BODY_LIMIT, heartbeat=30, receive_timeout=None
                ) as websocket:
                    bot = Bot(self, str(info["uin"]), ClientInfo())
                    self.bot_connect(bot)
                    delay = 1
                    async for message in websocket:
                        if message.type != aiohttp.WSMsgType.TEXT:
                            if message.type in (aiohttp.WSMsgType.CLOSE, aiohttp.WSMsgType.ERROR):
                                break
                            continue
                        try:
                            raw = json.loads(message.data)
                        except ValueError:
                            continue
                        parsed = self.parse_event(raw, info["uin"])
                        if parsed is None:
                            continue
                        event, lease = parsed
                        await self._slots.acquire()
                        task = asyncio.create_task(self._bounded_event(bot, event, lease))
                        self._event_tasks.add(task)
                        task.add_done_callback(self._event_tasks.discard)
            except asyncio.CancelledError:
                raise
            except Exception:
                logger.warning("qkbot bridge unavailable or catalog changed; reconnecting")
            finally:
                pending = list(self._event_tasks)
                for task in pending:
                    task.cancel()
                await asyncio.gather(*pending, return_exceptions=True)
                self.clear_pending_matchers()
                if bot:
                    with contextlib.suppress(Exception):
                        self.bot_disconnect(bot)
            await asyncio.sleep(delay)
            delay = min(delay * 2, 15)
