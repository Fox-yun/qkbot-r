"""A small OneBot V11 facade for explicitly selected upstream plugins.

It uses the official OneBot Bot/Event/Message classes. It opens no connection and
all supported APIs go through the same task-bound Milky gateway. Unsupported
operations fail visibly instead of returning a fabricated success.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from nonebot.adapters.milky.config import ClientInfo
from nonebot.adapters.milky.event import MessageEvent as MilkyMessageEvent
from nonebot.adapters.milky.exception import MilkyAdapterException
from nonebot.adapters.onebot.v11 import Adapter as OfficialAdapter
from nonebot.adapters.onebot.v11 import Bot, Message, MessageSegment
from nonebot.adapters.onebot.v11.event import GroupMessageEvent, PrivateMessageEvent
from nonebot.adapters.onebot.v11.exception import NetworkError
from nonebot.compat import type_validate_python

from .adapter import current_lease
from .transport import BridgeError


def positive(value: Any) -> int:
    if not isinstance(value, (int, str)) or isinstance(value, bool):
        raise BridgeError("invalid OneBot numeric identifier")
    try:
        number = int(value)
    except (ValueError, TypeError, OverflowError) as error:
        raise BridgeError("invalid OneBot numeric identifier") from error
    if number <= 0:
        raise BridgeError("invalid OneBot numeric identifier")
    return number


def media_uri(value: Any) -> str:
    if isinstance(value, bytes):
        import base64
        return "base64://" + base64.b64encode(value).decode("ascii")
    if not isinstance(value, (str, Path)):
        raise BridgeError("unsupported OneBot media source")
    text = str(value)
    if text.startswith(("base64://", "file://", "http://", "https://")):
        return text
    return Path(text).resolve().as_uri()


def outgoing(message: Any) -> list[dict]:
    if isinstance(message, list) and all(isinstance(item, dict) for item in message):
        message = Message(MessageSegment(item["type"], item.get("data", {})) for item in message)
    result = []
    for segment in Message(message):
        kind, data = segment.type, segment.data
        if kind == "text":
            result.append({"type": "text", "data": {"text": str(data["text"])}})
        elif kind == "at":
            result.append({"type": "mention_all", "data": {}} if str(data["qq"]) == "all"
                          else {"type": "mention", "data": {"user_id": positive(data["qq"])}})
        elif kind == "reply":
            result.append({"type": "reply", "data": {"message_seq": positive(data["id"])}})
        elif kind == "face":
            result.append({"type": "face", "data": {"face_id": str(data["id"]), "is_large": False}})
        elif kind in ("image", "record", "video"):
            result.append({"type": kind, "data": {"uri": media_uri(data["file"])}})
        else:
            raise BridgeError(f"OneBot message segment is not supported: {kind}")
    return result


def incoming(segments: list[dict]) -> Message:
    result = []
    for segment in segments:
        kind, data = segment["type"], segment.get("data", {})
        if kind == "text":
            result.append({"type": "text", "data": {"text": data["text"]}})
        elif kind == "mention":
            result.append({"type": "at", "data": {"qq": str(data["user_id"])}})
        elif kind == "mention_all":
            result.append({"type": "at", "data": {"qq": "all"}})
        elif kind == "reply":
            result.append({"type": "reply", "data": {"id": str(data["message_seq"])}})
        elif kind == "face":
            result.append({"type": "face", "data": {"id": data["face_id"]}})
        elif kind in ("image", "record", "video"):
            result.append({"type": kind, "data": {
                "file": data.get("resource_id", ""), "url": data.get("temp_url", "")
            }})
        elif kind == "forward":
            result.append({"type": "forward", "data": {"id": data.get("forward_id", "")}})
        else:
            # Preserve presence without pretending unsupported content is text.
            result.append({"type": kind, "data": dict(data)})
    return Message(MessageSegment(item["type"], item["data"]) for item in result)


def event_for(event: MilkyMessageEvent) -> GroupMessageEvent | PrivateMessageEvent:
    data = event.data
    message = incoming(data.segments)
    person = data.group_member if data.message_scene == "group" else data.friend
    sender = {
        "user_id": data.sender_id,
        "nickname": getattr(person, "nickname", ""),
        "sex": getattr(person, "sex", "unknown"),
    }
    if data.message_scene == "group":
        sender.update(card=getattr(person, "card", ""), role=getattr(person, "role", None))
    fields = {
        "time": event.time, "self_id": event.self_id, "post_type": "message",
        "message_type": "group" if data.message_scene == "group" else "private",
        "sub_type": "normal" if data.message_scene == "group" else "friend",
        "message_id": data.message_seq, "user_id": data.sender_id,
        "message": message, "raw_message": str(message), "font": 0, "sender": sender,
        "qkbot": event.qkbot,
    }
    if data.message_scene == "group":
        fields["group_id"] = data.peer_id
        return type_validate_python(GroupMessageEvent, fields)
    if data.message_scene != "friend":
        raise BridgeError("temporary/private-group sessions are not supported by the OneBot facade")
    return type_validate_python(PrivateMessageEvent, fields)


class Adapter(OfficialAdapter):
    def __init__(self, engine: Any):
        self.engine = engine
        super().__init__(engine.driver)

    def _setup(self) -> None:
        # Deliberately no WebSocket, webhook, HTTP listener or credential config.
        pass

    async def _call_api(self, bot: Bot, api: str, **data: Any) -> Any:
        del bot
        try:
            return await self._translate(api, data)
        except (BridgeError, MilkyAdapterException, LookupError, ValueError, TypeError) as error:
            raise NetworkError("OneBot operation is invalid or unavailable in the controlled bridge") from error

    async def _milky(self, action: str, params: dict) -> Any:
        return await self.engine.call_http(ClientInfo(), action, params)

    async def _translate(self, api: str, data: dict) -> Any:
        if "qkbot" in data:
            raise BridgeError("plugin-supplied identity is not accepted")
        if api == "get_login_info":
            result = await self._milky(api, {})
            return {"user_id": result["uin"], "nickname": result["nickname"]}
        if api in ("send_msg", "send_group_msg", "send_private_msg"):
            group = api == "send_group_msg" or (
                api == "send_msg" and (data.get("message_type") == "group" or "group_id" in data)
            )
            key = "group_id" if group else "user_id"
            result = await self._milky(
                "send_group_message" if group else "send_private_message",
                {key: positive(data[key]), "message": outgoing(data["message"])},
            )
            return {"message_id": result["message_seq"]}
        if api in ("upload_group_file", "upload_private_file"):
            group = api == "upload_group_file"
            key = "group_id" if group else "user_id"
            params = {
                key: positive(data[key]),
                "file_uri": media_uri(data["file"]),
                "file_name": str(data["name"]),
            }
            if group and data.get("folder"):
                params["parent_folder_id"] = str(data["folder"])
            await self._milky(api, params)
            return None  # Official OneBot upload result is null, not Milky's file ID.
        if api == "get_msg":
            lease = current_lease.get()
            event = lease.event
            if not isinstance(event, MilkyMessageEvent):
                raise BridgeError("message lookup requires an active source event")
            result = await self._milky("get_message", {
                "message_scene": event.data.message_scene,
                "peer_id": event.data.peer_id,
                "message_seq": positive(data["message_id"]),
            })
            message = result["message"]
            person = message.get("group_member") or message.get("friend") or {}
            return {
                "time": message["time"],
                "message_id": message["message_seq"],
                "message_type": "group" if message["message_scene"] == "group" else "private",
                "real_id": message["message_seq"],
                "sender": {"user_id": message["sender_id"], "nickname": person.get("nickname", "")},
                "message": incoming(message["segments"]),
            }
        if api in ("get_group_info", "get_group_member_info", "get_group_member_list"):
            params = {"group_id": positive(data["group_id"])}
            if api == "get_group_member_info":
                params["user_id"] = positive(data["user_id"])
            result = await self._milky(api, params)
            field = {"get_group_info": "group", "get_group_member_info": "member",
                     "get_group_member_list": "members"}[api]
            return result[field]
        raise BridgeError("OneBot API is not implemented by this facade")
