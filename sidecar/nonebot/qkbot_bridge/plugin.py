"""The only framework hooks: route matchers and retain ordinary got/pause sessions."""

from __future__ import annotations

from nonebot.adapters import Bot, Event
from nonebot.exception import IgnoredException
from nonebot.matcher import Matcher, matchers
from nonebot.message import run_postprocessor, run_preprocessor
from nonebot.rule import Rule
from nonebot.typing import T_State

from .adapter import Adapter, SESSION_MARKER, module_owner
from .transport import BridgeError

MARKER = "_qkbot_origin_task"


def engine_for(bot: Bot) -> Adapter:
    candidate = getattr(bot.adapter, "engine", bot.adapter)
    if not isinstance(candidate, Adapter):
        raise IgnoredException("qkbot accepts only its controlled adapters")
    return candidate


@run_preprocessor
async def require_owned_matcher(bot: Bot, event: Event, matcher: Matcher, state: T_State) -> None:
    del event
    engine = engine_for(bot)
    try:
        identity = engine.identity("matcher", matcher_required=True)
        binding = engine.session_binding()
    except BridgeError as error:
        raise IgnoredException("matcher does not own this event") from error
    if matcher.temp and matcher.state.get(SESSION_MARKER) != binding:
        raise IgnoredException("temporary matcher belongs to another connection or physical session")
    state[MARKER] = identity["task_id"]
    state[SESSION_MARKER] = binding


@run_postprocessor
async def preserve_pending_conversation(bot: Bot, matcher: Matcher) -> None:
    engine = engine_for(bot)
    identity = engine.identity("matcher", matcher_required=True)
    binding = engine.session_binding()
    # NoneBot itself creates a priority-0 temporary matcher for got/pause/reject.
    # Inspect its documented Matcher state rather than monkey-patching plugin handlers.
    pending = [
        candidate
        for candidate in matchers[0]
        if (
            candidate.temp
            and module_owner(candidate, engine.bindings) == identity["plugin_id"]
            and candidate._default_state.get(MARKER) == identity["task_id"]
            and candidate._default_state.get(SESSION_MARKER) == binding
        )
    ]
    if pending:
        async def same_session() -> bool:
            try:
                return engine.session_binding() == binding
            except BridgeError:
                return False

        # A Rule runs before NoneBot destroys its one-shot matcher. A foreign
        # account/session therefore cannot consume another pending conversation.
        for candidate in pending:
            candidate.rule &= Rule(same_session)
        await engine.extend_conversation(120)
