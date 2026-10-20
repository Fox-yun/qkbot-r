"""Render real upstream templates with an explicitly selected local browser.

No QQ connection, website request, browser download, or user browser profile.
Run separately from unit tests: python render_probe.py --browser <chrome.exe>
"""

import argparse
import asyncio
import base64
import io
import os
import sys
import tempfile
from pathlib import Path

import nonebot
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from qkbot_bridge.environment import configure_optional_runtime


async def render() -> None:
    from qkbot_echo import PNG
    import nonebot_plugin_htmlrender.browser as browser
    from nonebot_plugin_htmlrender import template_to_pic
    import nonebot_plugin_ddcheck.data_source as ddcheck
    import nonebot_plugin_acgnshow.acgnshower as acgnshow
    import nonebot_plugin_witff as witff

    playwright = None
    try:
        await browser.get_browser()
        playwright = browser._playwright
        # Each upstream render uses Browser.new_page, which creates a temporary
        # context. Block website assets, leaving local templates/data URLs usable.
        original_new_page = browser._browser.new_page

        async def local_page(**kwargs):
            page = await original_new_page(**kwargs)
            await page.route(
                "**/*",
                lambda route: route.abort()
                if route.request.url.startswith(("http://", "https://"))
                else route.continue_(),
            )
            return page

        browser._browser.new_page = local_page
        images = {
            "ddcheck": await ddcheck.render_ddcheck_image({
                "attentions": [], "attention": 0, "name": "本机测试", "mid": 123,
                "face": "data:image/png;base64," + base64.b64encode(PNG).decode(),
                "fans": 0,
            }, [], []),
            "acgnshow": await template_to_pic(
                str(acgnshow.RES_PATH), acgnshow.LIST_TEMPLATE_NAME,
                {"shows": [], "bgimage": acgnshow.choose_random_bgimage(),
                 "global_data": {"page": 1, "total_pages": 1, "total_results": 0}},
            ),
            "witff": await template_to_pic(
                str(witff.TEMPLATE_DIR), witff.HELP_TEMPLATE_NAME,
                {"examples": ["/兽聚 帮助", "/兽聚 文字发送"], "mode_hint": "本机模板测试"},
                pages={"viewport": {"width": 900, "height": 10}}, wait=0,
            ),
        }
        for name, data in images.items():
            with Image.open(io.BytesIO(data)) as picture:
                assert picture.format == "PNG"
                assert picture.width > 10 and picture.height > 10
                print(f"{name}: PNG {picture.width}x{picture.height} ({len(data)} bytes)")
    finally:
        await witff.client.aclose()
        await browser.shutdown_browser()
        if playwright is not None:
            await playwright.stop()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", type=Path, required=True)
    args = parser.parse_args()
    executable = args.browser.resolve()
    if not executable.is_file():
        parser.error("browser executable must already exist")
    previous = Path.cwd()
    with tempfile.TemporaryDirectory(prefix="qkbot-render-probe-") as directory:
        try:
            os.chdir(directory)
            options = {"htmlrender_browser_executable_path": str(executable)}
            nonebot.init(
                _env_file=(Path(directory) / "__no_dotenv__",),
                driver="~aiohttp", log_level="WARNING",
                localstore_cache_dir=Path(directory) / "cache",
                localstore_data_dir=Path(directory) / "data",
                localstore_config_dir=Path(directory) / "config",
                **options,
            )
            for module in (
                "nonebot_plugin_ddcheck", "nonebot_plugin_acgnshow", "nonebot_plugin_witff"
            ):
                assert nonebot.load_plugin(module), module
            configure_optional_runtime({"nonebot_plugin_htmlrender"}, options)
            asyncio.run(asyncio.wait_for(render(), timeout=60))
        finally:
            os.chdir(previous)


if __name__ == "__main__":
    main()
