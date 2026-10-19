# qkbot · Rust

面向小规模 QQ 群聊的轻量机器人：**Rust 单进程、SQLite、远程模型 API**。QQ 客户端与 Milky 协议端独立运行。

优先保证约定功能完整和运行可靠，再优化资源占用。不设固定 RSS 达标线，也不为压低内存数字删减记忆、上下文、插件能力或可靠性保护。轻量主要来自简单部署、复用连接和按需查询；容量与请求预算根据实际用途配置。

这是按 [qkbot](https://github.com/zclkkk/qkbot) 的插件思路重新实现的 Rust 项目，参考 [大肥鱼](https://github.com/SAKURAfan1023/dafeiyu-bot) 的身份归属、记忆证据与上下文控制规则。TypeScript / Fraq 插件需要移植。

## 已实现

- Milky WebSocket 事件与 HTTP API，登录账号核对、自动重连、消息去重、发送账本。
- 真实 `@`、关键词及白名单好友私聊触发；一层引用理解。
- 群共享 / 群内个人上下文，人设、输入预算、输出上限与历史长度控制。
- 近期消息、滚动摘要、带来源的长期记忆；SQLite FTS5 中文双字词检索，无向量服务。
- 静态源码插件：命令、事件、作用范围、初始化与退出；时钟与可选提醒作为参考实现。
- 原生模型工具协议，工具白名单、参数验证、本人确认、事务幂等；不执行模型正文里的指令或 JSON。
- 有界聊天 / 控制 / 数据库队列，有限并发，暂停、重置与遗忘使旧任务失效。

没有本地模型、Web 面板、主动接话、多模态、动态插件运行时或热重载。

## 启动

需要正在运行且支持 Milky 的协议端，以及兼容 Chat Completions 的模型接口。编译需要稳定版 Rust；Windows 使用 MSVC C++ Build Tools。SQLite 随程序编译，无须独立安装。

```powershell
# 当前工作区已有工具链；其他环境可直接使用 cargo。
.\scripts\cargo.ps1 build --release --locked
Copy-Item config.example.toml config.toml
```

编辑 `config.toml`：

1. 设置 `[milky]` 地址，建议填写 `bot_id`；有鉴权时设置 token。
2. 设置 `[llm]` 的完整 `chat_completions_url`、`model`。
3. 填写 `[access]` 的群号、好友 QQ 号和管理员 QQ 号。**空白名单不接收消息。**

```powershell
$env:QKBOT_API_KEY = "你的模型密钥"
# $env:QKBOT_MILKY_TOKEN = "你的 Milky token"
.\target\release\qkbot.exe --config config.toml --check
.\target\release\qkbot.exe --config config.toml
```

Linux / macOS 使用 `cargo build --release --locked` 和 `./target/release/qkbot`。不需要 Node.js 运行机器人；Node 只用于可选的本地性能冒烟脚本。

`--check` 只校验配置，不连接 QQ 或模型。环境变量覆盖文件中的密钥；相对数据库路径按配置文件目录解析。日志输出到 stderr，可交给服务管理器轮转。退出使用 Ctrl+C，Unix 也处理 SIGTERM。

## 配置取舍

`[plugins.ai.defaults]` 是全局默认，聊天指令保存当前会话的差异设置。白名单、管理员、工具能力及部署级队列和调用配额只在配置文件中调整。配置文件修改后重启生效。

`mode` 是新群的初始模式；已有群使用 `/ai mode` 切换，避免修改默认值时意外恢复旧会话。关闭记忆写入或暂停期间的新消息不会被标记为自动学习来源。

| 默认项 | 值 |
| --- | ---: |
| 模型 HTTP 并发 / 后台整理并发 | 2 / 1 |
| 全局 / 每会话待处理聊天 | 32 / 4 |
| 输入估算预算 / 输出上限 | 6000 / 800 |
| 近期消息 | 24 条 |
| 注入长期记忆 | 默认至多 5 条、1000 估算 tokens，可配置 |
| 每会话长期记忆容量 | 默认 256 条，可配置 |
| 单轮模型请求 / 工具调用 | 至多 3 / 2 |
| 单次模型 / 工具 / 整轮期限 | 60 / 15 / 120 秒 |
| 每会话发送额度 / AI 冷却 | 每小时 120 条 / 2 秒 |
| 普通消息保留期 | 7 天 |

预算使用保守的 UTF-8 字节估算，**不是模型 tokenizer 的精确计数**。工具定义、调用和结果都计入输入预算；问题本身超限会明确拒绝。普通聊天通常只请求模型一次，当前无隐藏重试。

`max_memories_per_conversation` 控制磁盘中的长期记忆容量；`memory_recall_limit` / `memory_recall_budget` 控制一次取回多少内容，提高容量不会把全部记忆装进上下文。降低容量不会删除既有记忆。输入预算可调至 262144、输出上限至 32768、历史至 4096 条；实际可用量仍取决于所选模型、消息发送限制与总输入预算。复杂工具任务可提高 `max_model_calls`、`max_tool_calls`、`max_http_attempts` 和整轮超时，保留有界执行及最终回答机会。

关键词检索是首版的低占用选择，换一种完全不同的说法可能漏召回。先收集真实漏召回案例，再决定是否加向量检索。群内 `user` 模式只隔离上下文，回复仍发到群里。

## 常用指令

```text
/bot status
/bot plugins
/bot confirm TOKEN
/bot reject TOKEN

/ai new                       清空短期上下文与摘要，保留长期记忆
/ai mode group|user            管理员切换群模式，开启隔离会话
/ai pause | resume | once      暂停 / 恢复 / 放行下一次触发
/ai context 6000
/ai output 800
/ai history 24
/ai persona list
/ai persona 温柔
/ai memory list [页码]
/ai memory remember 我不吃辣
/ai memory forget ID
/ai memory read on|off
/ai memory write on|off
/ai summary on|off
/ai tools
/ai status

/clock
/remind add 600 喝水
/remind list
/remind cancel ID
```

群共享设置由配置中的管理员管理；个人会话设置由本人管理。记忆查询和修改限本人、当前会话；`forget` 同时清空活跃上下文和摘要，撤销旧来源，避免旧任务重新学回。关闭自动记忆写入不影响显式 `remember`，摘要有独立开关。

`summary off` 同时停止生成和注入摘要，近期上下文改为按历史长度读取原始消息，长期事实仍可独立学习。再次开启摘要时，从仍保留的原始消息补齐进度，不重复提取已处理的事实；已过保留期的原文无法补回。重置、模式切换等内置控制指令先完成状态变更，再接收后续聊天，回执异步发送。

提醒插件默认关闭。启用 `[plugins.reminder].enabled` 后，普通 `/remind` 指令直接执行业务；若要让模型使用，还需把 `reminder_list`、`reminder_create`、`reminder_cancel` 加入 AI 的 `allowed_tools`。模型写操作等待本人在原聊天发送 `/bot confirm TOKEN`；5 分钟内有效，时间和内容已固定。等待确认不阻塞聊天，也不会再调用模型决定是否执行。

已创建的提醒可跨重启保留，未确认的请求在重启后失效。收到离线或换号事件后，暂停新发送与引用读取；原账号重新验证成功后，确定尚未提交的提醒恢复待发送，发送结果未知的提醒仍不自动重发。

## 扩展插件

在 `src/plugins/` 编写 `Plugin` 实现，在 `plugins/mod.rs` 显式装配。普通命令与模型工具复用业务方法；要提供给模型，实现 `Tool` 并注册，还须配置工具允许列表。新增工具无需修改 AI 主循环。`Plugin::accepts` 管命令和事件范围，`Tool::permits` 独立约束工具范围；本地 Rust 插件属于可信代码。

数据库迁移使用 `Store::migrate(plugin_id, version, sql)`，按插件记录校验和。数据库由专用线程持有；事务内只做短小的本地工作。所有发送使用 `Host::send`，网络结果未知时记账，不自动重发。

详细说明见 [架构与边界](docs/architecture.md)。

## 验证

```powershell
.\scripts\cargo.ps1 test --all-targets --locked
.\scripts\cargo.ps1 fmt --all --check
.\scripts\cargo.ps1 clippy --all-targets '--' '-D' warnings
```

测试使用临时 SQLite、本地 HTTP / WebSocket 模拟服务，不需要真实 QQ 账号或模型密钥。覆盖身份与引用、串线、队列顺序、重置取消、摘要与事实独立进度、复杂遗忘撤销、结构化工具协议、确认权限、插件异常回滚、重启与账号恢复、幂等及未知发送状态。

真实账号与远程 API 尚需部署时验证。内存数据用于了解运行成本和排查异常，不作为功能验收门槛；更长上下文、更多记忆或插件带来的合理资源增长可以接受。

基线发布构建的两次本机双群 20 条消息冒烟测试全部通过：空闲工作集 **9.42–15.68 MiB**，负载采样峰值 **10.27–16.12 MiB**。该基线早于容量可配置调整，使用固定 500 毫秒的本地模型模拟响应，未包含 QQ / Milky 或真实 API 的额外占用；[对应构建、测量条件与复现方法](docs/performance.md)。

SQLite WAL 运行时会产生 `-wal` / `-shm` 文件；备份请停机后复制或使用 SQLite 在线备份。不要只复制仍在写入的主文件。未知发送状态及来源撤销标记保留更久，不按普通日志到期删除。
