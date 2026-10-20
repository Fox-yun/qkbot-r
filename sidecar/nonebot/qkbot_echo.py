"""Small acceptance plugin. It knows no bridge token, task ID or transport API."""

import base64

from nonebot import on_command
from nonebot.adapters.milky import Message, MessageEvent, MessageSegment
from nonebot.params import CommandArg
from nonebot.plugin import PluginMetadata

__plugin_meta__ = PluginMetadata(
    name="桥接测试",
    description="验证真实 NoneBot/Milky 的文本、引用与图片发送",
    usage="/nbtest text 内容；/nbtest reply 内容；/nbtest image；/nbtest repeat 内容",
    supported_adapters={"~milky"},
)

echo = on_command("nbtest", priority=10, block=True)

# An intentionally tiny embedded PNG, not a screenshot or a remote image fetch.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII="
)


@echo.handle()
async def handle_echo(event: MessageEvent, args: Message = CommandArg()) -> None:
    text = args.extract_plain_text().strip()
    command, _, body = text.partition(" ")
    if command == "image":
        await echo.finish(MessageSegment.image(raw=PNG))
    elif command == "reply":
        await echo.finish(MessageSegment.reply(event.data.message_seq) + MessageSegment.text(body or "引用测试"))
    elif command == "repeat":
        await echo.send(body or "重复文本")
        await echo.finish(body or "重复文本")
    else:
        await echo.finish(body if command == "text" else (text or "NoneBot 桥接正常"))
