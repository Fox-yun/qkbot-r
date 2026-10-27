# qkbot-r · 小鱼

基于 Rust、SQLite 与 Milky 协议的 QQ 机器人，面向个人和小规模群聊部署。项目提供 AI 对话、个人记忆与关系、话题接续、Web 管理及可选扩展，通过远程模型 API 提供智能能力。

仓库维护三个功能范围不同的分支：`clean` 提供基础文本对话与记忆，`master` 提供完整群聊插件，`codex/embedding-stickers` 在主分支基础上持续增加双协议模型接口、个人记忆与话题接续、自动接话、经验学习、语义检索、群表情回复和联网搜索。部署前应先选择分支，再使用该分支的配置与启动方式。

[分支选择](#分支选择) · [LLBot 与部署准备](#llbot-与部署准备) · [clean 部署](#clean-分支部署) · [master 部署](#master-分支部署) · [增强分支部署](#增强分支部署) · [个人记忆与接续](#配置话题接续与个人记忆) · [首次配置](#首次配置与日常管理) · [更新与备份](#更新备份与分支切换) · [性能数据](#性能数据与测量范围) · [文档索引](#文档索引与来源)

## 分支选择

| 分支 | 定位与适用场景 | 部署方式 |
| --- | --- | --- |
| [`clean`](https://github.com/Fox-yun/qkbot-r/tree/clean) | 基础对话分支。适合只需要文本聊天、人设、长期记忆、时钟与提醒的场景，也可作为 Rust 插件开发基础 | 原生编译运行；该分支未提供容器构建与部署文件 |
| [`master`](https://github.com/Fox-yun/qkbot-r/tree/master) | 完整功能主分支。适合需要识图、群聊娱乐插件、图片菜单及可选 NoneBot 扩展的日常部署 | Linux rootless Podman，或原生编译运行 |
| [`codex/embedding-stickers`](https://github.com/Fox-yun/qkbot-r/tree/codex/embedding-stickers) | 增强分支。保留主分支功能，增加个人记忆与关系、跨场景接续、自动接话、经验笔记、双协议模型接口及向量 / 搜索能力 | Linux rootless Podman，或原生编译运行；Embedding 与 Tavily 按需配置独立 API |

以下能力按 **2026-10-27** 的分支源码核对：`clean` 为 `b649d3e`，`master` 为 `7847285`，增强分支的功能基准为 `175edff`。

| 能力 | `clean` | `master` | `codex/embedding-stickers` |
| --- | --- | --- | --- |
| 文本 AI、群共享 / 按人隔离上下文、人设 | 支持 | 支持 | 支持 |
| 聊天模型协议 | Chat Completions | Chat Completions | Chat Completions / Responses API 可选 |
| SQLite FTS5 记忆、摘要、证据管理与遗忘 | 支持 | 支持 | 支持 |
| Web 管理、群与好友开关、LLBot 扫码代理 | 支持 | 支持 | 支持 |
| 当前消息及明确引用中的图片识别 | 未提供 | 支持 | 支持 |
| 引用机器人消息触发、结构化引用 / @ 回复 | 未提供 | 支持 | 支持 |
| 日常经历与人物背景提炼、历史记忆重整命令 | 基础事实提炼，无重整命令 | 支持 | 支持 |
| 按 QQ 的跨会话记忆与近期话题接续 | 未提供 | 未提供 | 支持；默认群聊可接到本人私聊 |
| 按角色保存称呼、互动偏好、边界与交流记录 | 未提供 | 未提供 | 默认开启，可在「人物与关系」管理 |
| 输入压力触发的上下文压缩 | 未提供 | 未提供 | 支持，保留最近原文与完整问答 |
| 按群 / 好友开启自动接话 | 未提供 | 未提供 | 支持，各聊天默认关闭 |
| 可管理、可回滚的群聊经验笔记 | 未提供 | 未提供 | 可选，默认关闭，支持语义召回 |
| 13 项原生群聊插件与可选 NoneBot | 未提供 | 支持 | 支持 |
| Embedding 语义记忆、AI 群表情库 | 未提供 | 未提供 | 可选，需配置向量服务 |
| Tavily 联网搜索 | 未提供 | 未提供 | 可选，需配置搜索密钥 |
| AI 提醒创建 / 取消 | 需本人二次确认 | 需本人二次确认 | 新配置默认直接执行，可开启二次确认 |
| AI 根据上下文静默结束当前轮次 | 未提供该能力 | 未提供该能力 | 支持，不额外发起判断请求 |
| `Containerfile`、Podman 脚本、Quadlet | 未提供 | 提供 | 提供 |

三个分支的提醒插件均默认关闭，需先启用插件并允许对应模型工具。`master` 的手动表情保存插件与增强分支的 AI 表情检索、选图回复是不同功能。

## LLBot 与部署准备

### 公共依赖

三个分支都需要独立的 Milky 协议端和远程模型接口。`clean` 与 `master` 使用 Chat Completions；增强分支可在管理页选择 Chat Completions 或 Responses API，旧配置默认继续使用 Chat Completions，详见[模型协议配置](#配置模型协议)。SQLite 随 Rust 程序编译；聊天模型在远端调用，无需在服务器部署本地大模型。

本项目的扫码代理适配 **LLBot（LuckyLilliaBot）**。配套容器方案固定使用 `docker.io/linyuchen/llbot:8.3.2`，采用直连模式，无需在服务器另装桌面 QQ；首次登录仍需使用手机 QQ 扫码确认。运行模式与前置授权按 [LLBot 官方安装说明](https://luckylillia.com/guide/choice_install)配置。

| 目的 | 文档入口 |
| --- | --- |
| 安装 LLBot、选择运行方式 | [官方安装指南](https://luckylillia.com/guide/choice_install) |
| 下载版本与查看变更 | [GitHub 仓库](https://github.com/LLOneBot/LuckyLilliaBot) · [Releases](https://github.com/LLOneBot/LuckyLilliaBot/releases) |
| 配置 Milky 与 WebUI | [官方配置文档](https://luckylillia.com/guide/config) |
| 排查登录、连接与媒体问题 | [官方常见问题](https://luckylillia.com/guide/faq) |
| 使用本仓库的 LLBot 容器与登录恢复方案 | [master 部署指南](https://github.com/Fox-yun/qkbot-r/blob/master/docs/deployment.md#41-部署独立-llbot) · [增强分支部署指南](https://github.com/Fox-yun/qkbot-r/blob/codex/embedding-stickers/docs/deployment.md#41-部署独立-llbot) |

`clean` 分支未携带 LLBot 部署文件，使用官方安装方式独立部署协议端即可；它仍支持通过小鱼管理页扫码。

### 端口与凭据

| 服务 | 默认地址 | 凭据与配置 |
| --- | --- | --- |
| 小鱼管理页 | `http://127.0.0.1:3210` | 首次用初始化码设置管理员密码 |
| Milky HTTP / WebSocket | `http://127.0.0.1:3010` | `milky.url` 与 `milky.access_token` |
| LLBot WebUI / 扫码代理 | `http://127.0.0.1:3080` | `qq_login.url` 与 LLBot WebUI 密码 `qq_login.token` |
| 原生图片与 NoneBot 内部服务 | `127.0.0.1:3215` | 仅主分支与增强分支使用；NoneBot 使用独立桥接令牌 |

小鱼管理员密码、Milky 令牌、LLBot WebUI 密码与 LLBot 签名授权分别管理。按配套方案部署时，后三项分别保存于 `~/workspace/llbot/data/` 下的 `milky_token.txt`、`webui_token.txt`、`auth_token.txt`；签名授权由 LLBot 使用，不填入小鱼的 Milky 或 WebUI 密码框。

容器方案默认使用 Linux host 网络。Rust、LLBot 与可选 NoneBot 应共享回环网络，或部署在同一个 Pod，使协议端能够读取 `127.0.0.1:3215` 的临时图片。Windows / macOS 的 Podman 使用 Linux 虚拟机，host 网络对应虚拟机，需按[网络拓扑说明](docs/deployment.md#2-选择网络拓扑)安排组件位置。

下面三套流程均用于**首次安装，选择其中一套执行**。源码目录统一示例为 `~/workspace/qkbot-r`；容器方案默认使用镜像 `localhost/qkbot-r:latest`、容器 `qkbot` 和卷 `qkbot-data`。已有部署请先阅读[更新与分支切换](#更新备份与分支切换)，避免复用旧容器后误以为已切换版本。

## clean 分支部署

`clean` 保留文本 AI、群共享或按用户隔离的上下文、五套内置人设、带证据的长期记忆、摘要、时钟、可选提醒和 Web 管理。它支持一层引用文字理解，但不包含后续新增的识图、原生群聊插件、NoneBot、向量检索和联网搜索。

该分支采用**原生编译部署**，需要 Rust 工具链；Windows 还需要 MSVC C++ Build Tools。源码中没有 `Containerfile`、`deploy/podman.sh` 或 `deploy/*.container`，应使用以下命令。

### Linux / macOS

```bash
mkdir -p ~/workspace
cd ~/workspace
git clone --branch clean --single-branch https://github.com/Fox-yun/qkbot-r.git qkbot-r
cd qkbot-r

cargo build --release --locked
./target/release/qkbot --config config.toml
```

### Windows PowerShell

在准备存放源码的目录执行：

```powershell
git clone --branch clean --single-branch https://github.com/Fox-yun/qkbot-r.git qkbot-r
cd qkbot-r

.\scripts\cargo.ps1 build --release --locked
.\target\release\qkbot.exe --config config.toml
```

首次运行创建 `config.toml`，终端显示管理地址与一次性初始化码。按[首次配置](#首次配置与日常管理)填写 LLBot 和模型参数，保存并应用后，可发送 `/clock` 及带「小鱼」关键词的消息验证。该分支没有图片版 `/帮助` 菜单。

需要提醒时，启用 `plugins.reminder.enabled`，并在全局和当前会话允许 `reminder_list`、`reminder_create`、`reminder_cancel`。AI 创建 / 取消操作需本人在原聊天发送 `/bot confirm TOKEN`；普通 `/remind` 命令直接执行。

原生运行的数据默认位于配置文件目录：`config.toml`、`qkbot.sqlite3` 及 `.qkbot-admin/`。长期运行可由系统服务管理器托管可执行文件，工作目录、配置路径和启动用户应保持一致。完整说明见 [clean 使用文档](https://github.com/Fox-yun/qkbot-r/blob/clean/README.md)与 [clean 配置示例](https://github.com/Fox-yun/qkbot-r/blob/clean/config.example.toml)。

## master 分支部署

`master` 提供完整的群聊功能：在基础对话与记忆之外，加入图片识别、引用机器人消息触发、结构化引用 / @ 回复、日常记忆提炼、历史记忆重整、13 项原生插件及可选 NoneBot。

原生插件包括帮助菜单、随机选择、鹿管签到、Wordle、疯狂星期四、今日老婆、吃喝推荐、MC 查询、表情保存、戳一戳、模拟聊天图片、模拟转发和轮盘禁言。记忆检索采用 SQLite FTS5，该分支无需配置 Embedding 或 Tavily。

### Linux Podman

在已配置 rootless Podman 的 Linux 服务器执行；安装步骤见 [master 部署准备](https://github.com/Fox-yun/qkbot-r/blob/master/docs/deployment.md#1-准备-linux-主机)。

```bash
mkdir -p ~/workspace
cd ~/workspace
git clone --branch master --single-branch https://github.com/Fox-yun/qkbot-r.git qkbot-r
cd qkbot-r

sh deploy/podman.sh build
sh deploy/podman.sh start
sh deploy/podman.sh logs
```

镜像采用多阶段构建，运行层包含程序、证书、中文字体和 151 张内置餐图，以 UID 10001 运行。配置、管理员凭据、数据库与自定义资源持久化到 `qkbot-data` 卷的 `/data`。

完成[首次配置](#首次配置与日常管理)后，使用 `/帮助`、`/roll 米饭 面条 火锅` 与 AI 对话验证部署。图片识别需要支持视觉输入的聊天模型；`llm.vision` 默认开启，不支持视觉的模型应关闭该项。每轮最多读取当前消息与明确引用中的 4 张图片，图片会占用输入预算。

戳一戳、轮盘禁言、提醒与 NoneBot 预设默认关闭。按需启用插件后保存并应用；AI 提醒的创建和取消仍需本人二次确认。需要 B 站下载、多平台解析等上游功能时，另按 [master NoneBot 部署](https://github.com/Fox-yun/qkbot-r/blob/master/sidecar/nonebot/README.md#podman-部署可选)安装独立扩展进程。

Windows / macOS 或不使用容器的 Linux 主机，可在检出 `master` 后按[原生运行](#原生运行与开发检查)编译。自启动、升级、回滚与备份见 [master 运维指南](https://github.com/Fox-yun/qkbot-r/blob/master/docs/deployment.md)。

## 增强分支部署

完整分支名为 **`codex/embedding-stickers`**。该分支包含上述 `master` 功能，并增加：

- **双协议模型接口**：可选择 Chat Completions 或 Responses API，聊天、识图、工具调用和后台整理共用所选协议。
- **个人记忆与关系**：按机器人账号和 QQ 管理有来源的事实，按稳定角色保存称呼、互动偏好、边界和已确认的交流记录。
- **近期话题接续与压缩**：可接续本人参与但尚未得到回复的话题；按跨场景策略读取材料，并在输入压力较高时后台压缩上下文。
- **自动接话与经验笔记**：按群 / 好友选择是否让 AI 自然参与，另可启用有版本、来源及独立额度的群聊经验学习。
- **语义检索**：个人记忆与经验笔记可通过独立 Embedding API 建立向量索引，与文字召回结合；索引或服务不可用时保留文字召回。
- **AI 群表情回复**：按账号与群隔离表情库，支持自动收集、手动加入、管理预览、检索选图及随最终回复发送。
- **联网搜索**：通过 Tavily 获取摘要与来源，提供 `/搜索`、`/search` 和模型工具 `web_search`。
- **对话与提醒调整**：模型可依据上下文静默结束轮次；AI 提醒支持直接创建 / 取消或二次确认两种方式。

### Linux Podman

```bash
mkdir -p ~/workspace
cd ~/workspace
git clone --branch codex/embedding-stickers --single-branch https://github.com/Fox-yun/qkbot-r.git qkbot-r
cd qkbot-r

sh deploy/podman.sh build
sh deploy/podman.sh start
sh deploy/podman.sh logs
```

该分支与 `master` 使用相同的容器构建、网络和持久化方式。先按[首次配置](#首次配置与日常管理)完成普通聊天，再按需启用下面的能力。Embedding 与联网搜索默认关闭，没有对应密钥时仍可运行基础对话与原生插件。原生部署同样适用[编译运行流程](#原生运行与开发检查)。

### 配置模型协议

在管理页「模型接口 → 请求协议」选择协议，填写对应的完整地址、模型 ID 和密钥，保存并应用：

| 协议 | `llm.protocol` | 地址字段与常见路径 |
| --- | --- | --- |
| Chat Completions（默认） | `chat_completions` | `llm.chat_completions_url`，通常以 `/v1/chat/completions` 结尾 |
| Responses API | `responses` | `llm.responses_url`，通常以 `/v1/responses` 结尾 |

两个地址分别保存，模型与密钥共用；「获取模型列表」只补全当前协议地址。需按服务商实际能力选择协议、视觉输入和工具调用，失败时不会自动切换协议。旧配置省略 `llm.protocol` 时仍使用 Chat Completions。

两种协议均支持聊天、图片、原生工具与后台记忆 / 笔记整理。Responses 使用非流式请求、`store=false` 和本地上下文；切换协议保留已有记忆、关系和预算。向量接口仍单独配置，详见[模型接口说明](docs/user-guide.md#原生启动)。

### 配置话题接续与个人记忆

管理页「上下文与知识」集中提供「对话接续」「个人记忆」「人物与关系」和「经验笔记」。在「对话接续 → 调整接续与背景窗口」设置读取范围，对应 `[plugins.ai.context]`，保存并应用后生效。

近期接续默认开启：选取同一机器人账号下本人最近 2 小时的最多 3 个话题 / 问答片段；`include_unanswered = true` 允许读取本人已登记为可读的发言，无需等待机器人回复或长期记忆提炼。来源群背景默认前 10 条、后 5 条，前后各不超过 2 分钟，并共同服从预算、来源权限和撤回状态。旧历史不自动回填；多人自动接话不按最后一位发言者读取其私人资料。管理页预览也不代表每轮都会注入全部记录。

长期记忆默认以同一机器人账号下的 QQ 用户作为共享检索范围（`plugins.ai.global_memory_shared = true`）。**实际使用还受 `plugins.ai.context.cross_scene` 限制**，同样约束近期片段、个人记忆、关系资料及派生摘要和回答：

| `cross_scene` | 使用范围 |
| --- | --- |
| `private_followup`（默认） | 原场景可用，群聊材料可接到本人的私聊；私聊材料不进入群聊，群聊原文不跨群 |
| `same_scope` | 仅原群或原私聊 |
| `shared` | 显式允许同一用户跨群、私聊与群聊之间使用；仍校验来源和会话权限 |

个人记忆和关系按机器人账号与真实 QQ 隔离；昵称、引用和 @ 不改变材料归属。关闭 `global_memory_shared` 后，长期记忆仅在来源会话中检索；应用此开关变更会重置短期上下文与摘要，保留长期记忆和原始消息。需要保留旧版广泛跨场景行为的部署，应明确设置 `cross_scene = "shared"`；首次采用或切换受限策略时会重新整理旧摘要，停止复用无法追溯来源的旧助手回复，保留原始用户消息与长期知识。

「人物与关系」按 QQ 聚合个人事实、称呼、互动偏好、边界和近期确认交流，支持纠正、删除或重置当前角色的关系。关系默认开启，使用稳定的 `plugins.ai.personal.character_id = "default"`；修改人设文案不会重置关系，显式切换角色 ID 才使用独立关系。熟悉程度来自已确认的直接交流，同一场景每天最多记录一次。关系重置保留个人事实与原始聊天；事实的证据、编辑和永久遗忘在「个人记忆」中管理。

自动个人提炼逐项校验并保存有效候选，摘要失败不妨碍有效事实保存。失败项至少间隔 60 秒重新提炼，同一范围总计最多尝试三次；随后显示「待核对」并继续处理后续消息。可在工作区查看整理状态和证据，详细边界见[个人记忆与关系](docs/user-guide.md#个人记忆与关系)及[上下文实施说明](docs/context-memory-redesign.md)。

### 配置上下文压缩与经验笔记

在「上下文与记忆」调整 `[plugins.ai.compaction]`。自动摘要除原有条数与空闲触发外，会在裁剪前文字输入或待整理原文的估算量达到普通输入预算的 70% 时触发后台压缩，以 50% 为目标，默认保留至少最近 16 条原文及完整问答，仍受会话历史条数上限约束。摘要预算默认 2048，最短间隔 180 秒；前台聊天与模型额度优先，50% 是规划目标，最终请求仍受硬预算限制。关闭会话摘要后停止生成和注入，个人事实仍可独立提炼。详见[会话与记忆管理](docs/user-guide.md#会话与记忆管理)。

经验笔记默认关闭，在「学习笔记」设置中启用 `[plugins.ai.learning]` 并保存应用。它从符合学习条件的新群聊消息提炼经验、纠错和术语；私聊不作为取材来源，群背景取材还需开启 `learn_all_group_messages`。可在「上下文与知识 → 经验笔记」人工编辑、锁定、删除或回滚，保留来源和版本；通用笔记可在同账号的合格群聊间共享，群特定笔记仅在原群使用，实际注入仍受跨场景策略约束。

学习默认每小时最多 4 次、滚动 24 小时最多 24 次，整理间隔至少 900 秒。笔记每轮最多召回 8 条；启用全局 Embedding 后可做语义召回，未启用或不可用时回退文字匹配和最近笔记。笔记另有默认 12000 的聊天输入额度，最终输入估算上限为**普通输入预算 + 笔记额度**；个人记忆、关系和话题接续包含在普通输入预算内。配置时应核对总额是否适合所选模型，详见[学习笔记说明](docs/user-guide.md#学习笔记)。

### 配置语义记忆与群表情

1. 在管理页「向量接口（Embedding）」填写完整接口地址、模型、维度和独立密钥。当前配置模板使用 `https://api.siliconflow.cn/v1/embeddings`、`Qwen/Qwen3-VL-Embedding-8B`、1024 维；自定义接口需兼容项目的文字 / 图片向量请求格式。向量密钥不自动复用聊天模型密钥。
2. 点击「测试文字和图片向量」，核对返回维度。测试发送两次合成请求；启用「向量服务」并「保存并应用」后，后台逐步为已有有效记忆建立索引。已启用的学习笔记也可复用该接口建立独立索引。
3. 需要 AI 表情回复时，同时启用 `stickers.enabled`、`llm.vision` 和 `llm.tool_calling`，使用支持视觉与原生工具调用的聊天模型，并在全局及当前会话允许 `search_stickers` 和 `reply_message`。
4. 在表情库管理页核对收集与索引状态；可在群内引用一张图片并发送 `/sticker` 加入。模型检索本群候选后，每轮最多选择一张随最终回复发送。

`stickers.enabled` 与 `stickers.auto_collect` 默认开启，但表情库需要已启用的向量服务。自动收集仅处理允许服务的群中由协议明确标记为表情的图片；每群默认最多 256 项 / 128 MiB，全局最多 4096 项 / 512 MiB，同群表情回复间隔为 60 秒。

向量保存在 SQLite，表情文件保存在数据库同目录的 `stickers/`，容器默认路径为 `/data/stickers/`，无需另行部署向量数据库。启用服务后，待索引的记忆文本、检索文本及表情图片会发送至所配置的向量接口，备份时应保存完整数据目录。

### 配置联网搜索

1. 在管理页「联网搜索」填写 Tavily API 密钥，点击「测试搜索连接」。
2. 启用联网搜索并保存应用，用 `/搜索 查询内容` 验证命令入口。
3. 需要 AI 自行搜索时，启用模型原生工具调用，并在全局和当前会话勾选 `web_search`；已有会话的显式工具名单不会自动扩展。

搜索默认每轮最多调用一次、返回最多五条结果，使用有界超时与调用额度。回复中的来源链接来自实际检索结果。详细配置、额度与执行范围见[联网搜索说明](docs/web-search.md)。

### 配置自动接话与 AI 提醒

在「群与好友」逐项开启「自动接话」，即时保存并生效；**每个群和好友默认关闭**，新聊天不会自动继承。开启后，AI 可在普通消息中结合上下文选择参与或保持安静。全局策略和参数在「自动接话」设置中调整，对应 `[plugins.ai.auto_reply]`，保存并应用后只影响已开启的聊天。

默认连续消息等待 1.5 秒合并，最长等待 5 秒，每批最多 8 条；主动回复后冷却 30 秒，超过 120 秒的批次不再发送。@、关键词和有效引用触发优先；创建或取消提醒等写入操作需由明确触发发起。自动接话可能增加模型用量，编辑策略或人设时管理页会显示输入预算估算。

提醒插件仍需手动启用。该分支的新配置已将三个提醒工具加入允许列表，并将 `plugins.reminder.llm_writes_require_confirmation` 默认设为 `false`；启用插件后，可按用户请求直接创建或取消本人当前会话中的提醒。需要二次确认时，将该值设为 `true`。

从旧版本升级时，已有配置中的显式 `true` 会保留；原有全局和会话工具名单也需按需补选。仅切换分支或更换镜像不会自动改变这些设置。

该分支允许 AI 对没有新信息的应和、重复内容等静默结束本轮，不发送占位文字，也不增加一轮判断请求。@、关键词和引用机器人消息仍是触发入口，原生插件命令按各自规则执行。

## 首次配置与日常管理

三个分支的管理页默认监听 `127.0.0.1:3210`。本机部署直接访问[管理页](http://127.0.0.1:3210)；服务器部署时，在自己的电脑建立 SSH 转发，将 `your-server` 替换为实际 SSH 主机别名或 `用户@服务器地址`：

```bash
ssh -N -L 127.0.0.1:13210:127.0.0.1:3210 your-server
```

然后打开[转发后的管理页](http://127.0.0.1:13210)，依次完成：

1. 使用原生进程终端或 `sh deploy/podman.sh logs` 输出的一次性初始化码设置管理员密码。
2. 填写 Milky 地址与令牌、LLBot WebUI 地址与密码，保存配置。
3. 在「QQ 登录」获取二维码，使用手机 QQ 扫码确认。
4. 填写聊天模型接口、密钥和模型 ID；增强分支先选择请求协议，再填写对应地址。接口支持时可使用「获取模型列表」。
5. 选择人设、插件及模型工具，点击「保存并应用」，核对账号、运行状态与实际启用项。
6. 按所选分支的验证命令试用；增强分支的向量和搜索能力另行配置。

LLBot 尚未就绪时，可先初始化管理页并保存配置。扫码请求由小鱼服务端代理，远程管理只需转发 `3210`。

「保存当前配置」只保存文件，「保存并应用」才更新运行实例。群、好友及支持的会话插件黑名单即时保存生效；群与好友默认开启。人设支持预览、编辑、另存及按会话选择，切换后保留历史和记忆。

增强分支可在「上下文与知识」查看话题接续、个人事实、人物关系与经验笔记。先核对[跨场景使用范围](#配置话题接续与个人记忆)，再调整来源、预算或关系记录；默认允许群聊接到本人私聊，私聊资料进入群聊需要显式选择 `shared`。会话重置保留长期记忆，关系重置保留事实；永久遗忘应使用个人记忆中的预览与确认流程。

常用默认值为模型并发 2、后台整理并发 1、近期消息 24 条、普通输入 / 输出预算 6000 / 800、每轮至多 3 次模型请求和 2 次工具调用。输入使用保守的 UTF-8 字节与消息封装估算，并非模型 tokenizer 的精确计数；输出为模型 token 上限。增强分支的关系注入默认 2400、接续默认 3000，均包含在普通输入预算内；启用经验笔记后另加笔记额度。完整默认值以各分支的 `config.example.toml` 为准。

模型工具同时受全局允许列表、会话设置与插件开关约束。新模板增加的工具不会自动写入已有的显式名单；配置校验失败时，管理页显示原因，旧运行实例保持原配置。

### 密钥配置

| 环境变量 | 适用分支与用途 |
| --- | --- |
| `QKBOT_API_KEY` | 全部分支，聊天模型密钥 |
| `QKBOT_MILKY_TOKEN` | 全部分支，Milky 访问令牌 |
| `QKBOT_LLBOT_TOKEN` | 全部分支，LLBot WebUI 密码 |
| `QKBOT_NONEBOT_TOKEN` | `master` 与增强分支，NoneBot 桥接令牌，至少 32 字节 |
| `QKBOT_EMBEDDING_API_KEY` | 仅增强分支，独立向量密钥 |
| `QKBOT_SEARCH_API_KEY` | 仅增强分支，Tavily 搜索密钥 |

也可直接在管理页填写密钥，已有值不返回前端，留空保留原值。环境变量优先于配置文件，且须实际注入机器人进程；容器不会自动继承宿主 shell 中新增的变量。默认 Podman 脚本仅为聊天、Milky、LLBot 和 NoneBot 提供 secret 映射，增强分支的向量与搜索密钥可先通过管理页配置；使用环境变量时需自行配置容器或 Quadlet 注入。

## 原生运行与开发检查

三个分支均可原生编译。先使用对应分支的克隆命令取得源码，在项目根目录运行：

```bash
cargo build --release --locked
./target/release/qkbot --config config.toml
```

Windows PowerShell：

```powershell
.\scripts\cargo.ps1 build --release --locked
.\target\release\qkbot.exe --config config.toml
```

首次运行创建配置。已有配置可用 `--config config.toml --check` 校验，该操作不连接 QQ 或模型。Windows 编译需要 MSVC C++ Build Tools；SQLite 随程序构建，无需安装独立数据库服务。

`master` 与增强分支的图片绘制需要系统中文字体；原生餐图功能还应携带 `assets/native/meal/images`，放在工作目录或可执行文件旁，也可用 `QKBOT_MEAL_IMAGES_DIR` 指定目录。容器镜像已包含字体与餐图，`clean` 无需这些资源。

常规开发检查：

```bash
cargo test --all-targets --locked
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
node --test tests/admin_frontend.mjs
```

Windows 使用工作区工具链时，可将 `cargo` 替换为 `.\scripts\cargo.ps1`。Rust 核心运行无需 Node.js；此处 Node.js 用于前端测试和性能基准，独立 LLBot 的运行依赖由其部署方式提供。

## 更新、备份与分支切换

同一分支升级可先执行 `git pull --ff-only`，再按该分支方式构建。Podman 的 `start` 会复用已有容器，**重新构建镜像后仍需按升级流程重建容器**，才会运行新代码。脚本与 Quadlet 两种管理方式的停机、换镜像和回滚步骤见[部署文档](docs/deployment.md#8-升级与回滚)。

升级增强分支时，省略协议的旧配置继续使用 Chat Completions；新增的跨场景策略默认为 `private_followup`，关系模块默认为开启。请核对[个人记忆与接续配置](#配置话题接续与个人记忆)：需要沿用跨群或私聊到群聊的材料共享时显式选择 `shared`，仅接续完整问答时设置 `include_unanswered = false`。自动接话仍逐聊天开启，学习笔记仍默认关闭。

分支切换应按一次部署迁移处理：

1. 停止旧机器人并完整备份。原生版本保存配置、数据库及 `.qkbot-admin/`；容器版本导出整个数据卷，增强分支还需包含表情目录。LLBot 的授权、配置和登录态单独备份。
2. 在独立源码目录准备目标分支，以目标分支的配置模板为基础逐项迁移。不同分支的工具注册表、配置字段和数据库迁移存在差异，较新配置或数据库不能保证直接用于较旧分支。
3. 用独立配置、数据库副本或新数据卷验证目标版本，确认登录、历史、记忆和插件功能后再接管原服务。保留升级前数据以便回滚。

默认端口相同，变更容器名称并不能避免端口冲突；两个机器人也不应同时连接同一 QQ 账号处理消息。SQLite WAL 运行期间不要只复制主数据库文件，备份与恢复步骤见[运维说明](docs/deployment.md#9-备份与恢复)。

`master` 与增强分支还支持对仍保留的有效聊天原文进行历史记忆重整。备份后使用 `--rebuild-memory` 与 `--memory-rebuild-status` 管理任务；该操作可能调用模型，范围与命令见[历史记忆重整](docs/deployment.md#91-历史记忆重整)。`clean` 不提供这组维护命令。

## 性能数据与测量范围

当前归档中**没有三个分支最新版本在同一条件下的对比测试**。以下 Linux 数据来自 2026-10-20 的历史源码 `256dff38365dcb13be28e3d7537368198becd2af`，早于识图、向量检索、群表情、联网搜索、个人关系和跨场景接续等新增能力。

测试服务器为 Debian 13.7、2 vCPU、约 1.90 GiB 内存，使用 rootless Podman 5.4.2。基准加载当时的 12 项原生插件，执行双群共 20 条聊天、工具调用、4 张 PNG、两群记忆提炼与召回；模型由本地服务模拟，每次请求固定等待 500 ms。

| 历史测试指标 | Linux Rust 核心 |
| --- | ---: |
| 初始空闲 RSS 中位数 | 15.05 MiB |
| 双群聊天期间采样 RSS 峰值 | 16.33 MiB |
| 绘图与记忆流程后空闲 RSS 中位数 | 53.95 MiB |
| 进程生命周期峰值 RSS | 53.95 MiB |
| 聊天回复延迟中位数 / P95 | 505.50 / 508.79 ms |
| 截至末次采样的进程累计 CPU 时间 | 0.21 秒 |
| 发送账本确认 | 32 / 32 次 |
| 摘要 / 长期记忆 / 召回验证 | 2 / 2 / 2 |

这些数据仅统计机器人进程，LLBot、可选 NoneBot、浏览器、系统服务和开发工具应另计。内存采样峰值不代表生产上限，模拟模型耗时也不能作为真实模型延迟。完整环境、Windows 对照与原始数据见[性能报告](docs/performance.md)、[Linux 样本](docs/performance-podman-sample.json)和 [Windows 样本](docs/performance-sample.json)；`clean` 的早期基线见[该分支性能报告](https://github.com/Fox-yun/qkbot-r/blob/clean/docs/performance.md)。

`master` 与增强分支可在 Linux Podman 主机安装 Node.js 22.13 或更高版本后，运行现有隔离基准：

```bash
sh deploy/podman.sh build
node scripts/perf-smoke.mjs \
  --container-image localhost/qkbot-r:latest \
  --output target/perf-smoke/podman.json
```

基准使用独立配置、随机端口和数据库，保留 JSON 报告，通常约两分钟。现有脚本不覆盖向量服务、AI 群表情检索、Tavily 搜索，以及个人关系、跨场景接续和经验笔记的专项场景；新增能力的部署成本需另行测量。

## 文档索引与来源

| 分支 | 配置与使用 | 部署与架构 |
| --- | --- | --- |
| `clean` | [使用说明](https://github.com/Fox-yun/qkbot-r/blob/clean/README.md) · [配置示例](https://github.com/Fox-yun/qkbot-r/blob/clean/config.example.toml) | [架构说明](https://github.com/Fox-yun/qkbot-r/blob/clean/docs/architecture.md) |
| `master` | [使用手册](https://github.com/Fox-yun/qkbot-r/blob/master/docs/user-guide.md) · [配置示例](https://github.com/Fox-yun/qkbot-r/blob/master/config.example.toml) | [部署与运维](https://github.com/Fox-yun/qkbot-r/blob/master/docs/deployment.md) · [架构说明](https://github.com/Fox-yun/qkbot-r/blob/master/docs/architecture.md) |
| `codex/embedding-stickers` | [使用手册](https://github.com/Fox-yun/qkbot-r/blob/codex/embedding-stickers/docs/user-guide.md) · [配置示例](https://github.com/Fox-yun/qkbot-r/blob/codex/embedding-stickers/config.example.toml) · [联网搜索](docs/web-search.md) | [部署与运维](https://github.com/Fox-yun/qkbot-r/blob/codex/embedding-stickers/docs/deployment.md) · [架构说明](docs/architecture.md) |

增强分支的近期功能另见[个人记忆与关系](docs/user-guide.md#个人记忆与关系)、[上下文压缩](docs/user-guide.md#会话与记忆管理)、[学习笔记](docs/user-guide.md#学习笔记)和[统一上下文的实施现状与后续设计](docs/context-memory-redesign.md)。后者同时保留后续设计，已实现范围以文首状态与当前配置为准。

主分支与增强分支的功能来源、适配范围及资源许可另见[原生功能对照](docs/native-reference.md)、[群与媒体功能](docs/social-reference.md)、[帮助与图片渲染](docs/render-reference.md)、[NoneBot 适配](docs/nonebot-reference.md)和[资源来源与许可证](assets/native/README.md)。

本项目参考 [qkbot](https://github.com/zclkkk/qkbot) 的插件设计，以 Rust 重新实现核心与原生功能；提示词和记忆设计参考 [dafeiyu-bot](https://github.com/SAKURAfan1023/dafeiyu-bot)。TypeScript / Fraq 插件不直接加载，兼容范围以各分支实际实现和功能对照文档为准。
