# NoneBot 扩展

这个可选进程运行原版 NoneBot 插件，QQ 连接和会话权限仍由 Rust 宿主统一管理。未启用时无需 Python、浏览器或 ffmpeg。六项插件的真实版本、源码与功能差异见[源码核对](../../docs/nonebot-reference.md)。

## Podman 部署（可选）

Rust 主镜像不含 Python。只有需要原版 NoneBot 插件时才构建本目录的独立镜像；构建使用现有 `requirements*.lock` 和 `pip --require-hashes`，没有换用同名插件或重新解析业务版本。基础 Python 镜像和 Debian 系统包仍由其发行源提供；需要固定基础镜像时可传 `--build-arg PYTHON_IMAGE=完整镜像摘要`。

默认使用 PyPI 和 Debian 官方源。腾讯云服务器上若默认源连接缓慢，可仅在本次构建添加 `--build-arg PYTHON_IMAGE=mirror.ccs.tencentyun.com/library/python:3.11-slim-bookworm`、`--build-arg PYPI_INDEX_URL=https://mirrors.cloud.tencent.com/pypi/simple` 和 `--build-arg DEBIAN_MIRROR=http://mirrors.tencentyun.com`。Python 包继续校验锁文件中的 SHA-256，Debian 源继续使用镜像内原有签名密钥；不修改宿主全局源、不关闭 TLS 或包签名校验。构建参数只用于公共镜像地址，不要嵌入凭据。[腾讯云官方软件源说明](https://cloud.tencent.com/document/product/213/8623)

在仓库根目录构建。核心桥接验证只需：

```sh
podman build -f sidecar/nonebot/Containerfile \
  --build-arg PROFILE=core \
  -t localhost/qkbot-nonebot:latest .
```

六插件完整环境使用：

```sh
podman build -f sidecar/nonebot/Containerfile \
  --build-arg PROFILE=full \
  --build-arg WITH_BROWSER=1 \
  --build-arg WITH_FFMPEG=1 \
  --build-arg WITH_FONTS=1 \
  -t localhost/qkbot-nonebot:latest .
```

`PROFILE` 可取下表中的安装 profile，默认 `core`。三个 `WITH_*` 参数默认均为 `0`：浏览器用于查成分、漫展和兽聚图片；ffmpeg 用于媒体处理；字体安装 Noto CJK、彩色 Emoji 和 DejaVu，供中文图片渲染使用。完整示例显式包含这些依赖，镜像会明显更大。`WITH_BROWSER=1` 只适用于包含 Playwright 的 `web` 或 `full` 环境，浏览器在构建时用锁定的 Playwright 安装，运行时不会自行安装；这是避免浏览器与 Python 包版本不匹配的方式。[Playwright 官方容器说明](https://playwright.dev/python/docs/docker)

网络必须满足以下一种部署方式：

- **Linux 宿主网络**：Rust、LLBot 在同一 Linux 主机，sidecar 使用 `--network host`，都能访问同一 `127.0.0.1`。
- **同一 Pod**：Rust、LLBot、sidecar 全部加入 `qkbot` Pod，共享网络命名空间。桥端口 `3215` 无需发布到宿主；Web 端口只在创建 Pod 时发布。[Podman Pod 文档](https://docs.podman.io/en/latest/markdown/podman-pod-create.1.html)

普通容器网络中的三个容器不能仅靠服务名称连接：桥明确限制 loopback，发送给 LLBot 的媒体地址也是 loopback。Windows/macOS 的 Podman Machine 是独立 Linux 虚拟机，`--network host` 指向该虚拟机，无法直接复用 Windows/macOS 上 QQ 的 `127.0.0.1`。这种环境应把 Linux 协议实现一起放入同一 Pod；现有 Windows QQ/LLBot 应继续使用下节本机 Python 方式。

先在 Rust 管理页设置独立桥令牌、启用所需插件并保存应用，再将**相同令牌**放进 Podman secret。以下是 Linux Bash 的交互输入示例，令牌不会出现在命令参数中：

```sh
read -r -s -p 'NoneBot bridge token: ' QKBOT_NONEBOT_TOKEN
printf '\n'
printf '%s' "$QKBOT_NONEBOT_TOKEN" | podman secret create qkbot-nonebot-token -
unset QKBOT_NONEBOT_TOKEN
podman volume create qkbot-nonebot-data
```

Linux 宿主网络启动示例：

```sh
podman run -d --name qkbot-nonebot --init --network host \
  --stop-timeout 30 --shm-size 512m \
  --secret qkbot-nonebot-token,uid=10001,gid=10001,mode=0400 \
  -v qkbot-nonebot-data:/data:U \
  localhost/qkbot-nonebot:latest
```

同 Pod 部署时，将 `--network host` 换成 `--pod qkbot`；若 Pod 共享 IPC，把 `--shm-size 512m` 设在创建 Pod 时，并从此容器命令中删除。`512m` 是浏览器共享内存容量，并非整个容器的内存上限；按实际图片/浏览器负载调整。`--init` 用于回收浏览器子进程。容器内固定使用非 root 用户 `10001:10001`，`:U` 只调整这个独立命名卷的所有权。sidecar 不需要 Rust 数据库、管理台凭据目录或 LLBot 登录目录。

默认从 `/run/secrets/qkbot-nonebot-token` 读取令牌，也支持 `QKBOT_NONEBOT_TOKEN_FILE` 指定其他挂载文件；若使用环境变量，启动时通过 `--env QKBOT_NONEBOT_TOKEN` 传入已设置的值，不要把值写在命令或镜像构建参数中。显式 token 文件与 token 环境变量同时设置会被拒绝。[Podman secrets 文档](https://docs.podman.io/en/latest/markdown/podman-run.1.html#secret-secret-opt-opt)

可选插件配置先放入宿主受保护文件，再只读挂载到 `/config/plugins.toml`，并添加 `--env QKBOT_NONEBOT_PLUGIN_CONFIG=/config/plugins.toml`。其中路径必须是容器路径：完整镜像的浏览器已安装，不要复制 Windows 的 Chrome 路径；ffmpeg 使用 `/usr/bin/ffmpeg`。带 Cookie 的配置也可用 Podman secret 挂载。需要额外字体时，可将字体目录只读挂到 `/usr/local/share/fonts/custom`；字体自身许可证由提供者确认。

| 运行环境变量 | 默认值与用途 |
| --- | --- |
| `QKBOT_NONEBOT_PROFILE` | 随构建 profile；完整镜像可改为任一子集，不能在核心镜像中凭此启用未安装的模块 |
| `QKBOT_NONEBOT_URL` | `http://127.0.0.1:3215`，只允许 loopback HTTP |
| `QKBOT_NONEBOT_PLUGIN_CONFIG` | 不设置，不额外读取插件 TOML |
| `QKBOT_NONEBOT_CONCURRENCY` | `8`，同时处理事件数 |
| `QKBOT_NONEBOT_TASK_TIMEOUT` | `120` 秒，与 Rust 一侧对齐 |
| `QKBOT_NONEBOT_MEDIA_MAX_MIB` | `20`，与 Rust 一侧对齐，最大 `1024` |

`/data` 持久化缓存、插件数据与配置。容器构建时仅把 WITFF 原版 `prefs.json` 文件链接到 `/data/witff-prefs.json`，使其个人输出偏好随卷保留；不改写插件业务代码。停止或重建容器不会删除命名卷。升级时重建镜像并重建容器，继续挂同一卷；不要同时启动两个 sidecar。Rust 必须先启动并应用桥配置，sidecar 启动时会读取真实插件目录，桥未就绪会明确退出；修复后运行 `podman start qkbot-nonebot`。用 `podman logs qkbot-nonebot` 检查启动错误，停止用 `podman stop qkbot-nonebot`。

2026-10-20 已在 Debian 13 x86_64、rootless Podman 5.4.2 上构建并运行 `core`：Python 3.11.17，镜像约 197 MiB（Podman 报告的未压缩大小，含基础镜像；不是运行内存）。镜像 ID 为 `d34f3b555773b2d55b91fe8e2a273e66c610f2bb0756b670c190b1a01d5a21ec`。哈希锁安装及 `pip check` 通过；真实入口使用令牌文件与模拟桥建立认证 WebSocket、执行原版匹配器发送，并以退出码 0 响应 SIGTERM。还验证了 UID 10001、可写命名卷、容器重建后保留测试数据文件、无令牌时拒绝启动。入口单元测试 5 项通过。本轮仅验证 core 与数据持久化，未在 Linux 容器执行 WITFF 的业务偏好修改。

**full 镜像、Linux 浏览器渲染和真实 QQ 容器链路仍未实测**。core 验证不代表六个业务插件都完成 Linux 验收；部署后先使用 `nb_echo` 验证文本、引用和图片下载，再逐项启用业务插件。Python 插件及网页渲染仍是受信任代码执行，不应将此容器称为不可信插件的安全沙箱。

## 本机 Python 安装与启动

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

桥默认每文件 20 MiB，可在 Rust `nonebot.media_max_bytes` 与 Python `run.py --media-max-mib` 两端一起调整，最大 1 GiB。文件按 64 KiB 块上传；大文件不会按其完整大小加载进桥内存。它与上游下载限制相互独立：parser 原版默认下载上限 90 MiB，B站下载原版为 500 MB，超过桥额度的文件会明确失败。上传 HTTP 超时取任务时限与 300 秒的较小值，普通桥 API 为 30 秒；大文件任务可同时调大两端任务时限。上游选择 bytes/base64 时，其自身仍可能将内容完整载入 Python 内存。媒体图片、语音、视频、文件和组合转发均保留结构；不会将失败发送伪装成成功。LLBot 与桥须能访问同一 `127.0.0.1`；容器按上节共享网络，协议端位于另一台机器或独立网络命名空间仍不支持。

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

Linux 上可用已构建的 core 镜像运行容器入口检查。它在容器内部启动模拟桥，使用临时令牌，`--network none` 禁止外部网络；不需要 Rust 或 QQ 登录。以下测试卷专用于验收，不要换成运行中的数据卷：

```sh
podman volume create qkbot-nonebot-probe
podman run --rm --init --network none \
  -v qkbot-nonebot-probe:/data:U \
  -v "$(pwd)/sidecar/nonebot/tests/container_probe.py:/qa/container_probe.py:ro" \
  --entrypoint /opt/venv/bin/python \
  localhost/qkbot-nonebot:latest /qa/container_probe.py
podman volume rm qkbot-nonebot-probe
```

核心锁文件安装 29 个包，六插件完整环境 91 个包；这是安装规模，不是内存估值。浏览器启动、图片大小、并发媒体下载和 ffmpeg 是主要额外开销，完整 sidecar 通常远重于 Rust 核心。默认同时处理 8 个事件、单事件 120 秒；可用 `run.py --concurrency` 和 `--task-timeout` 调整，并与 Rust 任务超时对齐。尚未做真实 QQ 负载下的 RSS 压测，因而不承诺固定内存数字。
