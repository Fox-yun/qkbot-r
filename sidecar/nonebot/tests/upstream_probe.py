"""Exercise installed upstream handlers; mock only external website/download I/O."""

import asyncio
import itertools
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

import test_bridge as core
import nonebot
from aiohttp import web

original_cwd = Path.cwd()
work = tempfile.TemporaryDirectory(prefix="qkbot-upstream-probe-")
os.chdir(work.name)
config = nonebot.get_driver().config
nonebot.get_driver().register_adapter(
    core.Adapter,
    settings=core.Settings("http://127.0.0.1:9", "x" * 32),
    bindings={},
)
for name, value in {
    "localstore_cache_dir": Path(work.name) / "cache",
    "localstore_config_dir": Path(work.name) / "config",
    "localstore_data_dir": Path(work.name) / "data",
    "parser_render_type": "default",
    "parser_duration_maximum": 0,
    "parser_append_url": True,
    "parser_disabled_platforms": [
        "douyin", "kuaishou", "twitter", "youtube", "acfun", "tiktok", "weibo", "xiaohongshu", "nga"
    ],
}.items():
    setattr(config, name, value)

assert nonebot.load_plugin("nonebot_plugin_parser"), "installed parser must load without browser/ffmpeg"
assert nonebot.load_plugin("nonebot_plugin_bilibili_downloader")
assert nonebot.load_plugin("nonebot_plugin_ddcheck")
assert nonebot.load_plugin("nonebot_plugin_acgnshow")
assert nonebot.load_plugin("nonebot_plugin_witff")
assert nonebot.load_plugin("nonebot_plugin_voicemusic")

from nonebot_plugin_parser.matchers import register_parser_matcher
from nonebot_plugin_parser.parsers.bilibili import BilibiliParser, Video
from nonebot_plugin_parser.download import downloader
from nonebot_plugin_parser.config import pconfig
import nonebot_plugin_bilibili_downloader as download
import nonebot_plugin_ddcheck as ddcheck
import nonebot_plugin_acgnshow.acgnshower as acgnshow
import nonebot_plugin_witff as witff
import nonebot_plugin_voicemusic as music

register_parser_matcher()  # The real upstream on_startup matcher registration.


class UpstreamProbe(core.BridgeTests):
    message_numbers = itertools.count(1000)

    async def process(self, raw: dict) -> None:
        # Alconna caches message parsing by protocol message ID. Real messages
        # must have distinct sequence numbers even when separate tasks test them.
        raw["data"]["message_seq"] = next(self.message_numbers)
        await super().process(raw)

    async def asyncSetUp(self) -> None:
        await super().asyncSetUp()
        self.bindings.update(
            nonebot_plugin_parser="nb_parser",
            nonebot_plugin_bilibili_downloader="nb_download",
            nonebot_plugin_ddcheck="nb_ddcheck",
            nonebot_plugin_acgnshow="nb_acgnshow",
            nonebot_plugin_witff="nb_witff",
            nonebot_plugin_voicemusic="nb_music",
        )
        self.adapter.bindings = dict(self.bindings)
        self.adapter.onebot_modules.update({
            "nonebot_plugin_bilibili_downloader", "nonebot_plugin_witff", "nonebot_plugin_voicemusic"
        })
        self.adapter.settings = core.Settings(
            self.url, self.token, allowed_modules=frozenset(self.bindings),
            media_roots=(Path(work.name), self.root),
        )
        self.adapter.transport.settings = self.adapter.settings

    async def media(self, request: web.Request) -> web.Response:
        self.authenticated(request)
        self.assertIn(request.headers["X-Qkbot-Plugin"], self.bindings.values())
        self.assertEqual(request.headers["X-Qkbot-Generation"], "7")
        self.uploads.append(await request.read())
        return web.json_response({"url": self.url + "/media/capability", "media_id": "capability"})

    async def api(self, request: web.Request) -> web.Response:
        if request.match_info["action"].startswith("upload_"):
            self.authenticated(request)
            self.requests.append((request.match_info["action"], await request.json()))
            return web.json_response({"status": "ok", "retcode": 0, "data": {"file_id": "file-one"}})
        return await super().api(request)

    async def test_parser_real_matcher_video_metadata_and_cover(self) -> None:
        picture = self.root / "cover.png"
        picture.write_bytes(core.PNG)
        info = {
            "bvid": "BV1xx411c7mD", "title": "upstream fixture title",
            "desc": "fixture description " * 25,
            "duration": 60, "owner": {"mid": 55, "name": "author", "face": "https://fixture.invalid/avatar"},
            "stat": {name: 1 for name in ("view", "danmaku", "reply", "favorite", "coin", "share", "like")},
            "pubdate": 1, "ctime": 1, "pic": "https://fixture.invalid/cover",
        }
        with (
            patch.object(Video, "get_info", AsyncMock(return_value=info)),
            patch.object(BilibiliParser, "extract_download_urls", AsyncMock(return_value=("https://fixture.invalid/video", None))),
            patch.object(downloader, "download_img", AsyncMock(return_value=picture)),
            patch.object(downloader, "_download_file", AsyncMock(side_effect=AssertionError("basic profile must not download video"))),
        ):
            await self.process(core.event(
                "https://www.bilibili.com/video/BV1xx411c7mD", task="parser-video", plugin="nb_parser"
            ))
        sent = [data for action, data in self.requests if action == "send_group_message"]
        self.assertTrue(sent, "the real upstream parser matcher must send a result")
        self.assertIn("upstream fixture title", str(sent))
        self.assertTrue(
            any(segment["type"] == "forward" for data in sent for segment in data["message"]),
            "the original long-description forwarding path must preserve nested media",
        )
        self.assertTrue(self.uploads, "upstream cover output must pass the media bridge")
        self.assertTrue(all(data["qkbot"]["plugin_id"] == "nb_parser" for data in sent))

    async def test_download_real_onebot_cover_handler(self) -> None:
        async def download_file(url: str, path: Path):
            self.assertEqual(url, "https://fixture.invalid/cover")
            path.write_bytes(core.PNG)
            return True, ""

        with (
            patch.object(download, "get_video_info", AsyncMock(return_value={"pic": "https://fixture.invalid/cover"})),
            patch.object(download, "download_file", download_file),
        ):
            await self.process(core.event(
                "/封面图 BV1xx411c7mD", task="download-cover", plugin="nb_download"
            ))
        sent = [data for action, data in self.requests if action == "send_group_message"]
        self.assertEqual(len(sent), 3, "original progress, image and completion messages are preserved")
        self.assertEqual(self.uploads, [core.PNG])
        self.assertTrue(all(data["qkbot"]["plugin_id"] == "nb_download" for data in sent))

    async def test_parser_real_audio_command_file_and_reactions(self) -> None:
        audio = self.root / "parser-audio.mp3"
        audio.write_bytes(b"ID3" + b"\x00" * 64)
        with (
            patch.object(BilibiliParser, "extract_download_urls", AsyncMock(
                return_value=(None, "https://fixture.invalid/audio")
            )),
            patch.object(downloader, "download_audio", AsyncMock(return_value=audio)),
            patch.object(pconfig, "parser_need_upload", True),
        ):
            await self.process(core.event("/bm BV1xx411c7mD", task="parser-audio", plugin="nb_parser"))
        sent = [data for action, data in self.requests if action == "send_group_message"]
        self.assertEqual(sent[-1]["message"][0]["type"], "record")
        files = [data for action, data in self.requests if action == "upload_group_file"]
        self.assertEqual(len(files), 1, "Alconna's original optional file upload must work")
        self.assertEqual(files[0]["file_uri"], self.url + "/media/capability")
        self.assertEqual(self.uploads, [audio.read_bytes(), audio.read_bytes()])
        reactions = [data for action, data in self.requests if action == "send_group_message_reaction"]
        self.assertEqual([data["reaction"] for data in reactions], ["424", "144"])
        self.assertTrue(all(data["group_id"] == 456 for data in reactions))

    async def test_download_real_audio_file_upload(self) -> None:
        media = b"\x00\x00\x00\x18ftypM4A " + b"\x00" * 32

        async def download_file(url: str, path: Path):
            path.write_bytes(media)
            return True, ""

        with (
            patch.object(download, "get_video_info", AsyncMock(return_value={"title": "fixture audio"})),
            patch.object(download, "get_dash_streams", AsyncMock(return_value=("https://fixture.invalid/audio", None))),
            patch.object(download, "download_file", download_file),
        ):
            await self.process(core.event("/mp3 BV1xx411c7mD", task="download-audio", plugin="nb_download"))
        uploaded = [data for action, data in self.requests if action == "upload_group_file"]
        self.assertEqual(len(uploaded), 1)
        self.assertEqual(uploaded[0]["group_id"], 456)
        self.assertEqual(uploaded[0]["file_name"], "fixture audio.m4a")
        self.assertEqual(uploaded[0]["file_uri"], self.url + "/media/capability")
        self.assertEqual(self.uploads, [media])

    async def test_ddcheck_real_command_preserves_image_output(self) -> None:
        with (
            patch.object(ddcheck, "get_user_info", AsyncMock(return_value={"attentions": [1], "attention": 1})),
            patch.object(ddcheck, "get_vtb_list", AsyncMock(return_value=[{"mid": 1}])),
            patch.object(ddcheck, "get_medal_list", AsyncMock(return_value=[])),
            patch.object(ddcheck, "render_ddcheck_image", AsyncMock(return_value=core.PNG)) as renderer,
        ):
            await self.process(core.event("/查成分 123", task="ddcheck", plugin="nb_ddcheck"))
        renderer.assert_awaited_once()
        self.assertEqual(self.uploads, [core.PNG])
        sent = [data for action, data in self.requests if action == "send_group_message"]
        self.assertEqual(sent[-1]["qkbot"]["plugin_id"], "nb_ddcheck")

    async def test_acgnshow_original_command_and_shortcut(self) -> None:
        result = {"data": {"page": 1, "numPages": 1, "total": 0, "result": []}}
        with (
            patch.object(acgnshow, "get_regions_dict", AsyncMock(return_value={"上海": 1})),
            patch.object(acgnshow, "get_shows_data", AsyncMock(return_value=result)),
            patch.object(acgnshow, "template_to_pic", AsyncMock(return_value=core.PNG)) as renderer,
        ):
            await self.process(core.event("展览 上海", task="acgnshow-one", plugin="nb_acgnshow"))
            await self.process(core.event("上海展览", task="acgnshow-two", plugin="nb_acgnshow"))
        self.assertEqual(renderer.await_count, 2, "bare command and upstream shortcut must both work")
        self.assertEqual(self.uploads, [core.PNG, core.PNG])

    async def test_witff_real_text_and_image_modes(self) -> None:
        activity = {"title": "fixture gathering", "name": "fixture theme", "state": 2}
        with (
            patch.object(witff, "fetch_fusion_data", AsyncMock(return_value=[activity])),
            patch.object(witff, "template_to_pic", AsyncMock(return_value=core.PNG)),
            patch.object(witff, "PREFS_PATH", self.root / "preferences.json"),
            patch.object(witff, "USER_PREFS", {}),
        ):
            await self.process(core.event("/兽聚 文字发送", task="witff-text", plugin="nb_witff"))
            await self.process(core.event("/兽聚 图片发送", task="witff-image", plugin="nb_witff"))
        sent = [data for action, data in self.requests if action == "send_group_message"]
        self.assertIn("fixture gathering", str(sent))
        self.assertEqual(self.uploads, [core.PNG])
        self.assertEqual(sent[0]["message"][0]["type"], "mention", "upstream at_sender must be preserved")

    async def test_voicemusic_real_record_output_and_alias(self) -> None:
        audio = b"ID3" + b"\x00" * 64
        with (
            patch.object(music, "get_music_src", AsyncMock(return_value="https://fixture.invalid/audio")),
            patch.object(music, "download_audio", AsyncMock(return_value=audio)),
        ):
            await self.process(core.event("/点一首歌 fixture", task="music", plugin="nb_music"))
        sent = [data for action, data in self.requests if action == "send_group_message"]
        self.assertEqual(len(sent), 2)
        self.assertEqual(sent[-1]["message"][0]["type"], "record")
        self.assertEqual(self.uploads, [audio])


if __name__ == "__main__":
    suite = unittest.TestSuite(UpstreamProbe(name) for name in (
        "test_parser_real_matcher_video_metadata_and_cover",
        "test_parser_real_audio_command_file_and_reactions",
        "test_download_real_onebot_cover_handler",
        "test_download_real_audio_file_upload",
        "test_ddcheck_real_command_preserves_image_output",
        "test_acgnshow_original_command_and_shortcut",
        "test_witff_real_text_and_image_modes",
        "test_voicemusic_real_record_output_and_alias",
    ))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    # The plugin created its shared HTTP client at import. Close it explicitly
    # because this probe does not run NoneBot's full driver lifespan.
    asyncio.run(downloader.aclose())
    asyncio.run(witff.client.aclose())
    os.chdir(original_cwd)
    work.cleanup()
    raise SystemExit(not result.wasSuccessful())
