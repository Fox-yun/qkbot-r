# NoneBot 扩展

这个可选进程运行原版 NoneBot 插件，QQ 连接和会话权限仍由 Rust 宿主统一管理。未启用时无需 Python、浏览器或 ffmpeg。六项插件的真实版本、源码与功能差异见[源码核对](../../docs/nonebot-reference.md)。

## 安装与启动

使用 Python 3.11，在仓库根目录执行：

```powershell
.\sidecar\nonebot\install.ps1 -Profile full -Python python
```

脚本只创建 `data/nonebot-venv` 并安装哈希锁定的依赖，不修改全局 Python，不下载浏览器，不安装系统软件。`-EnvironmentPath` 可选择其他独立环境。每份锁文件固定了完整传递依赖；重复执行同一安装命令可以补齐安装。不要在此环境额外安装有冲突的插件；需要分开试验时使用不同环境路径。

| 安装 profile | 启动 profile | 内容 |
| --- | --- | --- |
| `core` | 不传 | 官方 NoneBot/Milky 与 `qkbot_echo` 测试插件 |
| `parser` | `parser-bilibili` | B站基础验证预设，关闭其他平台与视频下载，仍会下载封面 |
| `parser-full` | `parser-full` | 原版 parser 全平台与 yt-dlp 支持，保留上游默认配置 |
| `download` | `download` | B站下载与 OneBot 兼容层 |
| `web` | `web` | 查成分、漫展、兽聚，以及浏览器渲染库 |
| `music` | `music` | 原版语音点歌 |
| `full` | `full` | 上述六个业务插件，可分别开关 |

在管理页启用 NoneBot 桥和所需插件，设置独立桥令牌，然后保存并应用。六项默认注册预设在 [catalog.json](catalog.json)，初始全部关闭。启动时只加载 Rust 目录中已启用且被本地 profile 允许的模块；增加或删除模块后重启 sidecar。

轮盘禁言、戳一戳及其余十项 Rust 原生功能无需开启这个桥。如果原生插件已经勾选却仍提示「已停用」，先核对管理页的实际运行状态：仅保存不会应用配置；启用了 NoneBot 却没有有效令牌时，整份配置也无法应用。只使用原生功能时关闭「启用 NoneBot 兼容服务」再保存并应用；需要外部插件时补齐令牌后应用。插件卡片上的当前运行状态来自宿主实际加载结果，NoneBot 路由已启用仍需另外启动本节的 Python 进程。

在启动 sidecar 的终端中设置 `QKBOT_NONEBOT_TOKEN`，其值应与 Rust 的 NoneBot 桥令牌一致。这个令牌与 Milky、LLBot WebUI 和管理台密码不同。不要把真实令牌放进命令参数、仓库或截图。

```powershell
.\sidecar\nonebot\start.ps1 -Profile full
```

`start.ps1` 前台运行，Ctrl+C 退出。默认桥地址 `http://127.0.0.1:3215`，同时只接收一个 sidecar 连接。Rust 必须先启动；sidecar 不创建公网监听、不直接连接 QQ 协议端。Rust 不会自动安装依赖或启动此 Python 进程。

若仅验收桥接，可在 Rust 注册启用 `id="nb_echo"`、`module="qkbot_echo"`、`commands=["/nbtest"]`，安装 `core` 后不传 profile 启动。`/nbtest text 内容`、`/nbtest reply 内容`、`/nbtest image`、`/nbtest repeat 内容` 分别验证文本、引用、图片与两次相同内容的独立发送。

## 浏览器、媒体与插件配置

需要设置浏览器或上游插件参数时，先创建 `data/nonebot-plugins.toml`，再给启动命令添加 `-PluginConfig data\nonebot-plugins.toml`。这个可选文件使用顶层 TOML 键。例如：

```toml
# 使用已有浏览器的独立无头实例，不使用个人浏览器资料。
htmlrender_browser_executable_path = "C:/Program Files/Google/Chrome/Application/chrome.exe"

# 可选：只填写你实际持有的凭据，未使用的键直接省略。
# bilibili_cookie = "..."       # ddcheck
# parser_bili_ck = "..."        # parser
# parser_ytb_ck = "..."         # parser
# parser_xhs_ck = "..."         # parser
# superusers = ["你的QQ号"]      # 原版 /blogin 等权限
# bilibili_downloader_ffmpeg_path = "D:/tools/ffmpeg/bin/ffmpeg.exe"

# 可选：与 Rust 和 sidecar 的单文件额度对齐，避免下载后才发现超过发送上限。
# parser_max_size = 20
# bilibili_downloader_max_file_mb = 20
```

查成分、漫展和兽聚图片输出需要可运行的浏览器；兽聚也保留原版文字模式。可以指定已有 Chrome/Edge，或自行在隔离环境执行 `python -m playwright install chromium`。启动器禁止 htmlrender 在浏览器启动失败时自动执行系统依赖安装。退出时先撤销插件事件，再关闭浏览器及 Playwright 驱动。

B站下载的 `/mp3` 按原版发送 **M4A 文件**。`/mp4` 有 ffmpeg 时合并音视频，无 ffmpeg 时保留原版带声音的较低清晰度回退。parser 的完整音视频能力可能需要 ffmpeg、可访问目标网站的网络及相应 Cookie；全部十个平台仍注册，是否能取得具体内容取决于上游网站和插件。

VoiceMusic 保留 `/点歌` 与 `/点一首歌`，最终发送语音。其 `uin`、`skey` 会按原版实现提交给 `api.xingzhige.com` 的音乐服务；这不是 QQ 登录接口，也不应填写 Milky 或管理台凭据。此版本代码没有直接调用 ffmpeg，最终语音编码能力取决于 QQ 协议实现。

默认工作目录是 `data/nonebot-work`。LocalStore 缓存、数据与插件配置固定在其子目录，避免读取仓库 `.env`。媒体上传只允许缓存目录内的真实文件，或明确指定的 `run.py --media-root`；路径穿越、越出根目录的链接和宿主绝对路径转交均被拒绝。WITFF 原版将个人输出偏好写入隔离环境中自身包的 `prefs.json`，重新创建环境前如需保留，应单独备份该文件。

桥默认每文件 20 MiB，可在 Rust `nonebot.media_max_bytes` 与 Python `run.py --media-max-mib` 两端一起调整，最大 1 GiB。文件按 64 KiB 块上传；大文件不会按其完整大小加载进桥内存。它与上游下载限制相互独立：parser 原版默认下载上限 90 MiB，B站下载原版为 500 MB，超过桥额度的文件会明确失败。上传 HTTP 超时取任务时限与 300 秒的较小值，普通桥 API 为 30 秒；大文件任务可同时调大两端任务时限。上游选择 bytes/base64 时，其自身仍可能将内容完整载入 Python 内存。媒体图片、语音、视频、文件和组合转发均保留结构；不会将失败发送伪装成成功。LLBot 与桥须能访问同一 `127.0.0.1`，当前方案不支持协议端位于另一台机器或不同容器网络。

## 依赖与权限边界

完整环境使用 parser 默认的 PIL `common` 渲染器，保留十个平台、图文、音视频与转发功能。ddcheck 0.4.3 依赖 `htmlrender>=0.4,<0.5`；parser 的可选 `htmlrender` 扩展与 PicStatus 2.3.1 要求 `>=0.6.7`，不能放入同一锁定环境。这里没有把重依赖静默换成同名空功能。WITFF 0.1.7 的 PyPI wheel 缺少插件模块，安装使用作者固定提交的完整源码，见锁文件。

官方 Milky Bot、事件与消息模型原样使用。B站下载、WITFF、VoiceMusic 只声明 OneBot V11，因此使用进程内官方 OneBot Bot/Message/Event 外观层，将其实际 API 转换到同一受控 Milky 桥。它没有第二个 QQ 连接，不是通用 OneBot 服务；未实现的 API 明确报错。六项业务插件源码未被复制改写。

事件由 Rust 绑定任务、插件、运行代次和物理会话。`qkbot.generation` 是 Rust 生成的不透明字符串（当前为 UUID），HTTP/WS 与媒体请求头均原样传递，不能转成整数或自行构造。匹配器前置钩子只让所属插件执行；读消息、发送和媒体上传都带当前身份。每次 API 调用分配独立 operation ID，相同文本的两次合法发送不会被合并。网络错误不自动重发。普通 `got/pause/reject` 多轮会话可申请最多 120 秒的路由租约；事件结束后，复制 Context 的后台任务也失去发送资格。无当前事件的主动推送/定时发送尚未开放；例如 ddcheck 自身定时更新资料可以运行，但不能借此跨会话发送。

临时多轮匹配器另行绑定桥实例、登录账号、群/好友场景、会话、用户、插件和运行代次。在匹配规则和状态更新前都检查绑定；断线或停止时清理本桥的临时等待，不影响其他实例。旧账号或旧连接的等待不会被新命令接续；同一连接与物理会话中的正常问答保留原版行为。

Rust 的群/好友 Bot 开关、逐插件开关和最终发送校验始终生效。插件自己的权限只能进一步收紧。Python 插件是受信任的本地代码，这个桥不是 Python 安全沙箱：插件本身仍可访问网络、使用已授权目录和运行其依赖，安装新插件仍需审查。

## 验证与资源

以下测试只用临时目录和随机本机 HTTP/WS 端口，外部网站结果由固定样本代替，不会发 QQ 消息：

```powershell
& data/nonebot-venv/Scripts/python.exe -m unittest discover -s sidecar/nonebot/tests -p "test_*.py" -v
& data/nonebot-venv/Scripts/python.exe sidecar/nonebot/tests/upstream_probe.py
& data/nonebot-venv/Scripts/python.exe sidecar/nonebot/tests/render_probe.py --browser "C:/Program Files/Google/Chrome/Application/chrome.exe"
& data/nonebot-venv/Scripts/python.exe sidecar/nonebot/tests/rust_probe.py --bot-bin target/release/qkbot.exe
```

第一组验证认证、真实官方模型与匹配器、跨插件拒绝、租约、取消、发送身份、媒体边界、无重试与 HTTP/WS 传输。第二组调用已安装的六个原版插件处理器，验证文本、别名、快捷命令、图片、语音和文件路径。第三组真实渲染三个上游模板，阻断 HTTP/HTTPS 素材请求并关闭临时浏览器。第四组复制已编译的 Rust 二进制到临时目录，启动真实 Rust 与 `run.py` 两个进程，由本机 Milky 模拟端注入消息并接收发送，验证文本、引用、图片实际下载、两次相同文本及持久发送回执；管理页和 AI 均关闭，退出时关闭子进程并删除临时数据。`--bot-bin` 可指定其他已编译路径，测试不会自行构建。真实网站与账号权限仍需要部署后分别验收，不能从模拟测试推导出所有网站永久可用。

核心锁文件安装 29 个包，六插件完整环境 91 个包；这是安装规模，不是内存估值。浏览器启动、图片大小、并发媒体下载和 ffmpeg 是主要额外开销，完整 sidecar 通常远重于 Rust 核心。默认同时处理 8 个事件、单事件 120 秒；可用 `run.py --concurrency` 和 `--task-timeout` 调整，并与 Rust 任务超时对齐。尚未做真实 QQ 负载下的 RSS 压测，因而不承诺固定内存数字。
