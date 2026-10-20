# 小鱼 · qkbot-r

面向小规模 QQ 群聊的机器人：**Rust 核心、SQLite、远程模型 API、可选 NoneBot**。支持 Podman 部署，也能直接运行单个可执行文件。

轻量来自少依赖、按需加载和有界任务，不靠删减功能或设定死板的内存门槛。QQ 客户端与 Milky 协议端独立运行；Rust 核心不启动 Python、Chromium、向量数据库或本地大模型。

## 可以做什么

| 能力 | 已实现 |
| --- | --- |
| AI 聊天 | 群内真实 @、关键词、好友私聊；模型列表获取；人设列表切换；结构化工具调用与本人确认 |
| 上下文 | 群共享或群内按人隔离；近期消息、滚动摘要；输入预算、历史长度、模型与工具调用次数可调 |
| 长期记忆 | 带来源证据的 SQLite FTS5 检索；人工编辑、锁定、停用、遗忘；后台事实提炼与摘要独立控制 |
| 管理页 | 密码登录、LLBot 扫码代理、群与好友黑名单、每会话插件开关、工具勾选、保存与应用状态 |
| Rust 插件 | 帮助图片、鹿管签到、Wordle、疯狂星期四、今日老婆、吃喝图片、MC 查询、表情保存、戳一戳、模拟聊天图片、模拟转发、轮盘禁言 |
| 可选 NoneBot | 接入核对过的原版 B 站下载、查成分、多平台解析、漫展、兽聚、语音点歌插件 |
| 可靠性 | 账号与会话隔离、消息去重、发送账本、有限并发、任务超时、取消失效、权限校验、数据库迁移 |

群和好友默认开启，采用黑名单。**戳一戳、轮盘禁言及 NoneBot 预设默认全局关闭**，需要在管理页勾选并“保存并应用”。十二项 Rust 插件不依赖 NoneBot；误开未配置令牌的 NoneBot 会阻止配置应用，页面会显示原因并保留旧实例。

## Podman 快速部署

下面在 **Linux 服务器**执行，要求已安装 rootless Podman。默认使用主机网络，适合 LLBot 已在同一 Linux 主机监听 `127.0.0.1:3010/3080` 的情况。所有项目文件放在 workspace：

```bash
mkdir -p ~/workspace
cd ~/workspace
git clone https://github.com/Fox-yun/qkbot-r.git
cd qkbot-r

sh deploy/podman.sh build
sh deploy/podman.sh start
sh deploy/podman.sh logs
```

镜像多阶段编译，运行层包含程序、系统库、证书、中文字体和固定餐图库，使用非 root 用户。配置、管理员凭据、数据库与自定义插件资源保存在 `qkbot-data` 卷的 `/data` 中，升级镜像不会覆盖它们。未指定内存或 CPU 硬限制。

管理页默认仅在服务器本机监听。远程电脑运行：

```bash
ssh -N -L 127.0.0.1:13210:127.0.0.1:3210 你的SSH主机别名
```

浏览器打开 **http://127.0.0.1:13210**，用容器日志中的一次性初始化码设置管理员密码。在页面填写 Milky、模型 API 和 LLBot 登录管理配置。**LLBot 尚未部署时，管理页仍可初始化，QQ 聊天需等待协议端就绪。**

完整流程见 **[Podman 部署与升级](docs/deployment.md)**：包含 Debian 安装、同 Pod 部署、密钥、首次登录、持久化、systemd 自启动、备份恢复及从原生版本迁移。可选 Python 部分见 **[NoneBot 容器部署](sidecar/nonebot/README.md#podman-部署可选)**。

还未安装 QQ 协议端时，可按部署文档的 **LLBot 8.3.2** 章节安装配套容器：配置和登录态集中在 `~/workspace/llbot/`，由小鱼管理页代理扫码。提供独立 Quadlet 和登录恢复入口；WebUI、Milky 均只监听回环，通过 SSH 转发访问管理页。

> Windows/macOS 的 Podman 使用 Linux 虚拟机，`--network host` 指向虚拟机，不是 Windows/macOS 宿主。不要直接把 Windows 上 LLBot 的 `127.0.0.1` 配置照搬到 Linux 容器；Rust、NoneBot、LLBot 的回环地址需要处于同一个网络空间。

## 使用与配置

- **模型**：兼容 Chat Completions，支持从接口获取模型列表；已有密钥不会返回前端。密钥框留空保留原值。
- **人设**：内置五套成年男性角色，可预览、编辑、另存，并按会话切换；不会因此清空历史。
- **记忆**：近期上下文、摘要和长期事实分别管理。采用中文双字词检索；当前没有向量检索，换一种说法可能漏召回。
- **开关**：“保存当前配置”只写文件，“保存并应用”才更新运行实例。群／好友及会话内插件黑名单立即保存生效。
- **工具**：从实际工具注册表勾选。涉及写入的模型工具需本人在原聊天确认；模型正文不能冒充工具调用。
- **帮助**：`/帮助` 查看按当前会话筛选的卡片菜单，`/帮助 序号` 查看完整命令；自动换行和分页，图片与查询编号一致。
- **吃喝**：`早上吃什么`、`中午吃什么`、`晚上吃什么`、`今晚吃什么` 均可触发，也支持早餐／午餐／晚饭等说法和 `/` 前缀。内置 151 张原图随部署提供，发送不依赖 GitHub 外链；各时段共用菜品库。
- **权限**：机器人命令、模型工具与群管理权限分别检查。模拟聊天有明确标记；轮盘禁言还需要 QQ 群权限。

常用默认值：模型并发 2、后台整理并发 1、近期消息 24 条、输入预算 6000、输出上限 800、每轮至多 3 次模型请求和 2 次工具调用。预算是估算值，不是模型 tokenizer 的精确计数；这些值均可按需求调整。

详见 [配置与使用](docs/user-guide.md)、[完整示例配置](config.example.toml) 和 [架构与边界](docs/architecture.md)。

## 性能数据

2026-10-20 实测，全部 12 个 Rust 插件加载，执行双群 20 条聊天、工具调用、4 张图片、两群记忆提炼与召回。Linux 环境为 2 vCPU、约 1.90 GiB 内存的 Debian 13，rootless Podman 5.4.2；Windows 为 Ryzen AI 9 H 365。不同机器与系统指标不用于判断平台快慢。

| 机器人进程指标 | Windows 原生工作集 | Linux 容器内 RSS |
| --- | ---: | ---: |
| 初始空闲中位数 | 11.60 MiB | 15.05 MiB |
| 双群聊天采样峰值 | 13.64 MiB | 16.33 MiB |
| 绘图、记忆测试后空闲中位数 | 34.90 MiB | 53.95 MiB |
| 进程生命周期峰值 | 46.07 MiB | 53.95 MiB |

两端均完成 **32/32 次发送确认、2 份摘要、2 条长期记忆及 2 次召回验证**。该次基准的 Linux Rust 镜像约 **189.66 MiB**，包含中文字体；后续打包餐图增加约 10.75 MiB 原图资源，当前镜像大小以构建结果为准。可选 NoneBot core 镜像约 **197.2 MiB**，不含六个上游功能及浏览器。镜像大小是磁盘指标，不是运行内存。

这些是模拟模型每次等待 500 ms 的隔离测试，数字不包含 QQ／LLBot、NoneBot、浏览器、模型服务或容器运行时，也不是生产内存上限。完整环境、逐阶段延迟、CPU、原始样本与复现命令见 **[性能报告](docs/performance.md)**。

## 原生运行与开发

```bash
cargo build --release --locked
./target/release/qkbot --config config.toml
```

Windows 可使用 `.\scripts\cargo.ps1 build --release --locked`，运行 `.\target\release\qkbot.exe`。首次启动创建配置，管理页默认 http://127.0.0.1:3210。SQLite 随程序编译，无需另装数据库。

原生部署餐图功能时同时携带 `assets/native/meal/images`，可放在工作目录或可执行文件旁；也可用 `QKBOT_MEAL_IMAGES_DIR` 指向该目录。容器已包含图库，无需额外下载。该变量一旦设置便只使用指定目录，配置错误不会悄悄改用其他来源。

```bash
cargo test --all-targets --locked
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
node --test tests/admin_frontend.mjs
```

运行核心无需 Node.js；Node 只用于开发测试与可复现基准。测试使用独立数据库和本地模拟协议／模型，不依赖真实 QQ 或模型密钥。

## 实现与来源

这是参考 [qkbot](https://github.com/zclkkk/qkbot) 插件思路重新实现的 Rust 项目，提示词与记忆设计参考 [dafeiyu-bot](https://github.com/SAKURAfan1023/dafeiyu-bot)。不会直接加载 TypeScript／Fraq 插件。

原生插件按实际上游实现核对后移植；外部平台频繁变化的功能通过 NoneBot 运行原版插件。兼容范围和差异分别记录在：

- [原生功能对照](docs/native-reference.md)
- [群与媒体功能对照](docs/social-reference.md)
- [帮助菜单与图片实现](docs/render-reference.md)
- [NoneBot 上游与适配说明](docs/nonebot-reference.md)
- [资源来源与许可证](assets/native/README.md)
