"""Small environment/lifecycle constraints, without replacing plugin handlers."""

from pathlib import Path
import re
import sys
from typing import Any
from urllib.parse import quote

from .transport import BridgeError


def redact_log(text: str, secrets: tuple[str, ...]) -> str:
    for secret in secrets:
        if secret:
            text = text.replace(secret, "[redacted]").replace(quote(secret, safe=""), "[redacted]")
    return re.sub(
        r"(?i)([?&](?:skey|key|token|access_token|password|cookie)=)[^&\s'\"<>]+",
        r"\1[redacted]",
        text,
    )


def configure_logging(token: str, options: dict[str, Any]) -> None:
    from nonebot.log import default_filter, default_format, logger
    secrets = (token, *(str(value) for key, value in options.items()
                       if isinstance(value, str) and re.search(
                           r"key|token|cookie|password|secret|_ck$", key, re.IGNORECASE
                       )))
    logger.remove()
    logger.add(
        lambda message: sys.stderr.write(redact_log(str(message), secrets)),
        level=0, filter=default_filter, format=default_format, diagnose=False,
    )


def configure_optional_runtime(loaded: set[str], options: dict[str, Any]) -> None:
    if "nonebot_plugin_htmlrender" not in loaded:
        return
    import nonebot_plugin_htmlrender.browser as browser

    async def explicit_install_only() -> None:
        raise BridgeError(
            "browser could not start; install Playwright Chromium explicitly or set "
            "htmlrender_browser_executable_path in the sidecar plugin config"
        )

    # Upstream 0.4.0 otherwise runs system install-deps after any launch error.
    # This is an environment hook; template rendering itself remains unchanged.
    browser.install_browser = explicit_install_only
    executable = options.get("htmlrender_browser_executable_path")
    if executable and not Path(executable).is_file():
        raise BridgeError("configured renderer executable does not exist")

    from nonebot import get_driver

    @get_driver().on_shutdown
    async def close_renderer_driver() -> None:
        # htmlrender 0.4.0 closes Chromium but leaves the Playwright driver alive.
        # This final-registered hook runs first; its original shutdown is a no-op
        # afterwards. It also handles a partially failed browser launch.
        playwright = browser._playwright
        try:
            await browser.shutdown_browser()
        finally:
            if playwright is not None:
                await playwright.stop()
