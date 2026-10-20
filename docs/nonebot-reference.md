# NoneBot 上游实现与兼容核对

核对日期：2026-10-20。此处区分实际运行的上游业务代码、桥接协议和仍依赖外部环境的能力。安装和测试方法见 [sidecar README](../sidecar/nonebot/README.md)；六项默认禁用预设来自 [catalog.json](../sidecar/nonebot/catalog.json)。

## 基础接口

固定 NoneBot2 **2.5.0**、Milky 适配器 **1.3.0**、OneBot 适配器 **2.4.6**。NoneBot 的 `run_preprocessor` / `run_postprocessor`、`current_matcher` 和临时 matcher 机制用于校验插件归属及多轮租约。Milky 官方模型允许额外事件字段，因此顶层 `qkbot` 身份可以保留；业务插件仍收到官方事件类型。仅替换适配器的连接与 `call_http`，不改事件/消息/API 模型。[NoneBot 固定源码](https://github.com/nonebot/nonebot2/tree/c0f9a494bcd308c2ec530ba60c167f9145d21aec)，[Milky 固定源码](https://github.com/nonebot/adapter-milky/tree/be631a186de7d18f488bb54e8a3aa807cd72f226)，[OneBot 固定源码](https://github.com/nonebot/adapter-onebot/tree/7194dbb9d363d152230ecb9a7225125999a0e6af)。

Milky 客户端使用真实 `get_login_info` 响应中的 `uin/nickname`。`get_message`、群/成员/好友查询保持各自官方数据包裹；发送结果保留 `message_seq/time`。事件保留真实 `friend/group/group_member`，Alconna/uninfo 的人物与角色来自这些信息，不生成虚假管理员身份。

## 六项业务插件

| 插件与安装版本 | 核对到的真实功能 | 接入方式与必要条件 |
| --- | --- | --- |
| bilidownloader-woju **0.3.0** | `/mp3` 下载 M4A 文件；`/mp4` 下载视频文件；`/封面图` 发进度、图片、完成信息 | 原版 OneBot V11；映射群/好友发送与文件上传。ffmpeg 可合并音视频，缺少时原版回退流畅有声 MP4。 |
| ddcheck **0.4.3** | `/查成分 UID/用户名`，查询关注与粉丝牌，渲染成分图；定时更新 VTuber 数据 | 原版 Alconna，支持 Milky；需要 HTMLRender 浏览器，部分 B站查询需要 Cookie。 |
| parser **2.6.10** | 十个平台图文、音视频、媒体转发；B站 BV/av、动态、直播、收藏、专栏；`/bm`、`/ym`、`/blogin`、开启/关闭解析 | 原版 Alconna/Milky；完整环境安装 yt-dlp，保留默认 PIL 渲染。Cookie、网络、ffmpeg 按上游需要配置。 |
| acgnshow **0.3.1** | 裸命令 `展览 地区 [页码] [日期]`、`地区展览` 快捷形式、`展览详情 ID`；列表与详情图片，可选网页详情 | 原版 Alconna/Milky；真实快捷正则进入预设，浏览器依赖按需安装。 |
| WITFF **0.1.7 源码** | `/兽聚` 关键词、分页、帮助、文字/图片切换、个人偏好 | 原版 OneBot V11；上游直接依赖 HTMLRender 但未在包元数据声明，锁文件补齐。PyPI wheel 缺少模块，改装固定 GitHub 源码。 |
| VoiceMusic **0.1.20** | `/点歌` 与 `/点一首歌`，取得歌曲链接、下载音频并发送 QQ 语音 | 原版 OneBot V11；依赖作者指定第三方音乐服务。`uin/skey` 会提交至该服务，不是 QQ/Milky 认证。代码没有直接启动 ffmpeg。 |

对应已核对的官方固定源码：

- [bilidownloader-woju](https://github.com/Wojusensei/nonebot-plugin-bilidownloader-woju/tree/fdc63d50f975a8360dc2a29a8354f7fd7eb326fe)：`nonebot_plugin_bilibili_downloader/__init__.py` 的三个处理器、下载和 `send_file`，`config.py`。
- [ddcheck](https://github.com/noneplugin/nonebot-plugin-ddcheck/tree/a8439bff1f38f91756981571c37c792c1e7fce46)：命令注册、`data_source.py` 查询及截图、模板、`pyproject.toml`。
- [parser](https://github.com/fllesser/nonebot-plugin-parser/tree/4aa5364432193db701d71fad9c71ae768c951490)：`src/nonebot_plugin_parser/parsers`、`matchers`、`helper.py`、渲染与下载模块。NGA 已存在于代码中，不能只按 README 表格漏掉。
- [acgnshow](https://github.com/LiteyukiStudio/nonebot-plugin-acgnshow/tree/942a0f5d087fddd2dee2435e6fedbcaa29c9bf3b)：`acgnshower.py` 的命令/shortcut/详情路径、API 包装与模板。安装发行版号为 0.3.1，源码注释中的其他版本号不作为依赖版本。
- [WITFF](https://github.com/Kelei327/nonebot-plugin-witff/tree/eac461885caaf69591171b2a39fac006f213890b)：`__init__.py` 查询、过滤、分页、偏好与模板。未改写上游 handler，直接安装此提交。
- [VoiceMusic](https://github.com/Onimaimai/nonebot-plugin-voicemusic/tree/5ce6c55168c4d3c239b342ab6a75b692c8b51bc2)：`__init__.py` 的命令、全局互斥、音乐 API 和音频发送，`config.py`。

固定提交用于说明源码审查依据；除 WITFF 直接锁定该源码归档外，其余安装使用对应 PyPI 发行包的完整哈希锁。未宣称这些包与任意更新后的默认分支自动兼容。

parser 的十个平台是 B站、抖音、快手、微博、小红书、YouTube、TikTok、X、AcFun、NGA。预设域名与源码中的解析入口对应；独立 BV/av 使用与上游一致的正则。分享卡片仍由原版提取 `meta.detail_1.qqdocurl`、`meta.news.jumpUrl`、`meta.music.jumpUrl`。`/开启解析`、`/关闭解析` 还受原版 `to_me` 和超级管理员/群管理员权限限制；`/blogin` 只允许上游超级管理员私聊。

## 桥接范围

OneBot 外观层只给声明需要它的原版模块派发同一事件；检查过三个模块均不依赖 `get_bot/get_bots` 选择另一条 QQ 连接。实际需要的文本、@、引用、表情、图片、语音、视频和群/好友文件上传映射到受控 Milky API；不支持的操作明确失败。它不是对任意 OneBot 插件的通用兼容承诺。

Milky 路径保留图片、视频、语音、文件与组合转发的结构。parser 使用的三个 reaction ID 为 `424`、`144`、`10060`，通过当前群及触发消息绑定的操作发送。文件/字节资源先流式上传为短期 capability URL，再交 Milky；不把 sidecar 绝对路径直接发送给协议端。

每次操作携带 Rust 发出的任务、插件、运行代次与独立 operation ID。读权限、发送权限、群/好友开关及插件开关最终由 Rust 判断，NoneBot 原版权限可以进一步限制。后台无事件的发送明确拒绝；多轮等待由有期限租约支持，断开或事件失效不能延续旧任务发送。

## 依赖冲突和验证边界

ddcheck 0.4.3 要求 `nonebot-plugin-htmlrender>=0.4,<0.5`，parser 2.6.10 可选 HTML 渲染扩展要求 `>=0.6.7`，不能合并安装。完整六项使用 parser 默认 PIL 渲染和 htmlrender 0.4.0，未禁用其平台或媒体功能。基础 `parser-bilibili` 仅是明确标注的验证预设。

WITFF 0.1.7 的 PyPI wheel 只有 dist-info，没有可导入的插件代码。安装锁固定作者 GitHub 提交归档并校验哈希，同时补上作者代码真实导入而未声明的 HTMLRender 依赖。WITFF 偏好文件仍保留原版包内存储位置。

AccessControl 与 PicStatus 是单独参考项，前者不取代 Rust 的最终权限校验；PicStatus 2.3.1 的 HTMLRender 版本与上述六项联合环境冲突。完整说明见[辅助插件源码核对](render-reference.md)。

已执行官方模型/匹配器与本机 HTTP/WS 回归，以及全部六项真实 handler 的模拟网站回归；三个上游 HTML 模板还通过真实 Chrome 截图测试。另以复制的真实 Rust 二进制和真实 `run.py` 做跨进程端到端验证，模拟 Milky WS 注入事件，检验文本/引用/图片/repeat、图片资源实际下载和 SQLite 中确认回执；运行代次是 Rust 提供的不透明字符串，Python 原样传回。外部网站调用、实际 QQ 发送、真实 Cookie 有效性和真实群聊负载不在这些本机回归的证明范围内。浏览器、ffmpeg 与网络服务的资源消耗属于可选 sidecar，Rust 主程序不承担常驻 Python/浏览器开销。
