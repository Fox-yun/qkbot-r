"""Test fixture, deliberately absent from the production module allow list."""

import asyncio

from nonebot import on_command
from nonebot.adapters.milky import Bot, Message, MessageEvent
from nonebot.params import CommandArg

BACKGROUND_RESULTS: list[object] = []
BACKGROUND_GATE: asyncio.Event | None = None
background = on_command("background", priority=10, block=True)
conversation = on_command("conversation", priority=10, block=True)


@background.handle()
async def detached_send(bot: Bot, event: MessageEvent) -> None:
    async def later() -> None:
        assert BACKGROUND_GATE is not None
        await BACKGROUND_GATE.wait()
        try:
            await bot.send(event, "late")
        except Exception as error:
            BACKGROUND_RESULTS.append(error)
        else:
            BACKGROUND_RESULTS.append("unexpected send")

    asyncio.create_task(later())


@conversation.handle()
async def begin() -> None:
    await conversation.send("next?")


@conversation.got("answer")
async def finish() -> None:
    await conversation.finish("done")
