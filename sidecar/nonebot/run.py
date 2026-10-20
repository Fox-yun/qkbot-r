"""Start from an isolated working directory; no public listener is created."""

from __future__ import annotations

import argparse
import asyncio
import os
import sys
import tomllib
from pathlib import Path

import nonebot

from qkbot_bridge.adapter import Adapter
from qkbot_bridge.environment import configure_logging, configure_optional_runtime
from qkbot_bridge.transport import BridgeError, Settings, Transport


def main() -> int:
    parser = argparse.ArgumentParser(description="qkbot controlled NoneBot sidecar")
    parser.add_argument("--url", default="http://127.0.0.1:3215")
    parser.add_argument("--allow-module", action="append", default=None)
    parser.add_argument("--profile", action="append", default=[])
    parser.add_argument("--plugin-config", type=Path)
    parser.add_argument("--work-dir", type=Path, default=Path(__file__).resolve().parents[2] / "data/nonebot-work")
    parser.add_argument("--media-root", type=Path, action="append", default=[])
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--task-timeout", type=float, default=120)
    parser.add_argument("--media-max-mib", type=int, default=20)
    args = parser.parse_args()
    try:
        work = args.work_dir.resolve()
        work.mkdir(parents=True, exist_ok=True)
        modules = set(args.allow_module or ["qkbot_echo"])
        plugin_config: dict = {}
        profile_dir = Path(__file__).resolve().parent / "profiles"
        for name in args.profile:
            if not name.replace("-", "").isalnum():
                raise BridgeError("invalid profile name")
            profile = tomllib.loads((profile_dir / f"{name}.toml").read_text(encoding="utf-8"))
            modules.update(profile["modules"])
            plugin_config.update(profile.get("nonebot", {}))
        if args.plugin_config:
            if args.plugin_config.stat().st_size > 65536:
                raise BridgeError("plugin configuration exceeds 64 KiB")
            overrides = tomllib.loads(args.plugin_config.read_text(encoding="utf-8"))
            forbidden = {
                "driver", "milky_clients", "milky_webhook", "onebot_ws_urls", "onebot_access_token",
                "log_level", "command_start", "session_expire_timeout", "_env_file",
            }
            if forbidden.intersection(overrides):
                raise BridgeError("plugin config cannot replace the controlled transport")
            plugin_config.update(overrides)
        cache = work / "cache"
        cache.mkdir(exist_ok=True)
        plugin_config.update(
            localstore_cache_dir=cache,
            localstore_data_dir=work / "plugins-data",
            localstore_config_dir=work / "plugins-config",
        )
        settings = Settings(
            url=args.url,
            token=os.environ.get("QKBOT_NONEBOT_TOKEN", ""),
            allowed_modules=frozenset(modules),
            media_roots=(cache, *(root.resolve() for root in args.media_root)),
            media_max_bytes=args.media_max_mib * 1024 * 1024,
            concurrency=args.concurrency,
            task_timeout=args.task_timeout,
        )

        async def load_catalog() -> dict[str, str]:
            transport = Transport(settings)
            try:
                return await transport.catalog()
            finally:
                await transport.close()

        bindings = asyncio.run(load_catalog())
        os.chdir(work)
        configure_logging(settings.token, plugin_config)
        nonebot.init(
            _env_file=(work / "__no_dotenv__",),
            driver="~aiohttp",
            log_level="WARNING",
            milky_clients=[],
            milky_webhook=None,
            command_start={"/"},
            session_expire_timeout=120,
            **plugin_config,
        )
        driver = nonebot.get_driver()
        driver.register_adapter(Adapter, settings=settings, bindings=bindings)
        adapter = driver._adapters["Milky"]
        if nonebot.load_plugin("qkbot_bridge.plugin") is None:
            raise BridgeError("bridge hooks failed to load")
        for module in bindings:
            plugin = nonebot.load_plugin(module)
            if plugin is None:
                raise BridgeError("configured plugin failed to load; check its installed dependencies")
            supported = plugin.metadata.supported_adapters if plugin.metadata else None
            if supported and any(name.endswith("onebot.v11") for name in supported) and not any(
                name.endswith("milky") for name in supported
            ):
                adapter.onebot_modules.add(module)
            elif supported and not any(name.endswith("milky") for name in supported):
                raise BridgeError("configured plugin does not support Milky or the OneBot V11 facade")
        configure_optional_runtime(
            {plugin.module_name for plugin in nonebot.get_loaded_plugins()}, plugin_config
        )
        # NoneBot shuts down hooks in reverse registration order. Revoke plugin
        # events before the plugins close browser/HTTP resources. _stop_bridge is
        # idempotent, so its original adapter hook remains safe.
        driver.on_shutdown(adapter._stop_bridge)
        nonebot.run()
        return 0
    except (BridgeError, OSError, ValueError) as error:
        print(f"NoneBot sidecar: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
