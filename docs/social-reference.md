# 社交、媒体与 MC 插件来源核对

本次实现按用户确认的具体仓库检查源码，独立实现 Rust 版本。下列提交是核对时的固定版本；功能边界来自实现代码，不以插件名称猜测行为。MC、资源读取及群操作使用 Milky 的标准 API。

## 来源与核对范围

| 功能 | 原项目 / 固定提交 | 核对源码 | 许可 |
|---|---|---|---|
| MC 服务器信息 | [nikissXI/nonebot_plugins](https://github.com/nikissXI/nonebot_plugins/tree/485ac68ac80566eb800e7388421bbf556793a2b7/nonebot_plugin_mc_server_status) | `nonebot_plugin_mc_server_status/__init__.py`、`config.py`、`pyproject.toml` | `pyproject.toml` 声明 MIT；该目录未提供独立 LICENSE |
| 今日老婆 | [glamorgan9826/nonebot-plugin-today-waifu](https://github.com/glamorgan9826/nonebot-plugin-today-waifu/tree/84f5243c7e50b7444dee241ac242a5fe4d6488ad) | `__init__.py`、`record.py`、`config.py`、`utils.py` | MIT |
| 表情包保存器 | [colasama/nonebot-plugin-sticker-saver](https://github.com/colasama/nonebot-plugin-sticker-saver/tree/5433f6d0240aedfbdf8aa2dff4f6aca3c77ec266) | `nonebot_plugin_sticker_saver/__init__.py` | MIT |
| 戳一戳 | [Agnes4m/nonebot_plugin_poke](https://github.com/Agnes4m/nonebot_plugin_poke/tree/a20e9c4596d45a8a4bcb5de2cd5aa88fb2e3a6e5) | `__init__.py`、`matcher.py`、`utils.py`、`config.py` | MIT |
| 模拟转发 | [Cvandia/nonebot-plugin-fakemsg](https://github.com/Cvandia/nonebot-plugin-fakemsg/tree/655e979c8d5e8b2029639c7df6cbf8141d6b3873) | `nonebot_plugin_fakemsg/__init__.py`、`config.py` | MIT |
| 轮盘禁言 | [KarisAya/nonebot_plugin_russian_ban](https://github.com/KarisAya/nonebot_plugin_russian_ban/tree/de805231bd1ec1672989df33a210ff5fa65af762) | `nonebot_plugin_russian_ban/__init__.py`、`utils.py` | MIT |

Milky 字段核对源：[官方 common.ts](https://github.com/SaltifyDev/milky/blob/main/protocol/src/ir/common.ts)、[消息 API](https://github.com/SaltifyDev/milky/blob/main/protocol/src/ir/api/message.ts)、[系统 API](https://github.com/SaltifyDev/milky/blob/main/protocol/src/ir/api/system.ts)、[群 API](https://github.com/SaltifyDev/milky/blob/main/protocol/src/ir/api/group.ts)、[好友 API](https://github.com/SaltifyDev/milky/blob/main/protocol/src/ir/api/friend.ts)。

## 今日老婆

保留每天按群记录、当天重复查询同一人选、排除自己和已被抽到的人、没有候选时回退机器人。换人默认开启、最多两次；超过次数后当天不再获得人选。保留活跃优先、自动双向、自动撤回、群管理员配置和机器人管理员重置。头像与群名片/昵称随人选显示，身份由 QQ 号确定。群数据额外按机器人账号隔离。

命令包括 `今日老婆`、`换老婆`、`今日老婆信息`、`今日老婆帮助`、`开启/关闭换老婆`、`设置换老婆次数 N`、`开启/关闭自动撤回`、`设置自动撤回延迟 N`、`开启/关闭自动设置对方老婆`、`设置抽取模式 随机模式/活跃模式`、`设置活跃天数 N`、`重置/刷新今日老婆`，以及 `/partner` 子命令。

新增个人退出与重新参与。退出使涉及该人的当天配对失效，重新参与不会恢复旧配对；退出名单不会被重置命令清除。活跃候选必须仍在当前群，避免原代码中历史活跃用户离群后仍被选中的情况。活跃窗口按最近 N 天的真实群消息时间计算，最多 365 天；换人次数最多 100，撤回延迟最多 300 秒。原版使用运行机器本地日期，此版本统一使用机器人配置时区。

撤回仅在发送获得有效消息序号后安排，受运行时取消、插件版本、会话开关和当前账号检查约束。断线后不自动重发或重试结果未知的动作。

## MC 服务器信息

原版 `/信息` 查询当前群绑定的多台服务器；`/信息数据` 查看管理信息；管理员可添加、删除、测试服务器。Java 与 Bedrock 均支持。Java 返回版本、人数、可用玩家样本、匿名人数和可用服务器图标；Bedrock 返回版本与人数。原版没有定时轮询推送。

本版本保留两种协议及按群绑定，新增 `/mc 名称`、`/mc list`。地址、端口和类型由 Web 管理员配置，聊天不接受任意网络地址；机器人管理员可用 `/mc bind 名称`、`/mc unbind 名称` 调整当前群绑定，或通过配置的 `group_ids` 设置初始绑定。`添加服务器 群号 名称`、`删除服务器 群号 名称` 只操作当前群已配置目标；`测试服务器 名称` 只测试配置中的目标。这是明确的管理入口变化，不再允许聊天临时指定任意地址。

每次最多四个查询并发、单目标总超时五秒。Java 状态包不超过 64 KiB，Bedrock 数据包不超过 4 KiB。不执行 Minecraft 控制命令，不访问 RCON。端口显式配置，不自动进行 SRV 服务发现。查询错误不向群内回显内部地址解析或网络异常详情。

## 表情保存

原版不是持久化贴图库，而是回复一条表情后用 `.save` / `保存表情` 等命令提取第一张图片，重发为普通图片并附原始链接。本版本保留这一路径，提供 `/save`、`.save`、`/sticker`、`/保存图片`、`/保存表情`、`/保存`。

通过真实被引用消息的 `resource_id` 获取临时资源 URL，要求机器人账号、群/私聊、目标消息序号全部匹配。不把任意文字 URL 当成图片，不透传 `file://`。不沿用原版的第三方短链接域名替换；直接提供 QQ 临时链接并说明其有效期。

## 戳一戳

保留真实戳事件且接收者必须为机器人，支持黑/白名单、文本、图片、语音和可选回戳。默认仅文本；同时启用语音和图文时按原规则随机选择其一。管理员可配置 `resource_dir/pic`、`resource_dir/acc`、`resource_dir/poke.txt`；文本中的“我”替换为配置昵称。

`/zq`、`/抓图` 可收集当前消息或回复消息的图片。群员收集的图片仅用于当前会话，管理员本地资源目录可作为共享资源。新增会话冷却防止重复事件刷屏。默认文案使用一组简短回复，不包含“定位用户地址”等虚假能力宣称；自定义 `poke.txt` 可覆盖。

资源只能来自已验证的 QQ 消息或管理员配置目录。路径先规范化并检查目录边界，单个本地文件最多 20 MiB，拒绝 SVG 和未知文件签名；实际媒体服务仍执行自身容量上限。一次抓图最多八张，当前会话图库最多 128 张。

## 模拟转发

保留 `QQ号说内容|QQ号说内容`、真实 `@成员说内容`、每人消息分隔符、用户分隔符与受保护 QQ 名单。原代码的元数据描述与实现有差异，例如实际按空格拆分同一人的多条消息，未按描述把所有空格一律拒绝；此版本按实现解析并增加长度、条数限制。身份必须属于当前群，私聊只允许当前双方。

与聊天图片插件的裸 `QQ说` 规则存在重叠，统一消歧：含 `|` 或首段真实 `@` 的形式作为合并转发；单条裸 `QQ说` 交给聊天图片。单条转发可显式使用 `/mock-forward QQ号说内容`。还支持 `/mock-forward 名字:内容 | 名字:内容`。

始终在外层标题、第一条说明及每个模拟发送者名称中标注“模拟/非真实记录”，不会伪装成真实聊天证据。不解析 CQ 码，不根据昵称合并身份。最多 16 条用户内容，正文总输入最多 8192 字节。

## 轮盘禁言

保留群共享六发弹仓、一颗子弹、每次空枪推进一格、`重新装弹` / `拨动滚轮` 重新随机，以及命中后结束当前局。`开枪` 本身表示主动参与，命中仅对这次开枪的人禁言，原随机时长为 2–6 分钟，且受 Web 配置上限限制。插件默认关闭；群内普通成员能否开始游戏由群管理员开关控制，管理员可直接开始。

保留 `自由轮盘`、`轮盘禁言`、`开枪`、`重新装弹`、`拨动滚轮`、`开启/关闭自由轮盘` 等命令，并提供 `/roulette` 子命令。不加入“join 后再抽签”的另一套玩法。结束一局保留群管理员设置，避免原代码删除整份群状态后连开关也丢失。

同时保留原包的管理员命令：`禁言[时长][单位] @成员`，默认五分钟，支持秒/s、分钟/min、小时/h、天/d、月/个月/M，单个中文数字一至十也可用。管理员指定时长上限为协议支持的三十天，游戏的 `mute_max_secs` 仅约束轮盘命中时长。`解封`、`解禁`、`解除禁言` 携带真实 @ 时直接解除；不携带 @ 时先列出正在被禁言的成员，再接收同一管理员在同群内的一条 QQ 号选择，多个号码用空格分隔。选择有效期为 120 秒，仅允许选择刚才列出的成员，权限变化后失效。原项目没有全群禁言命令，本版本也不添加此操作。

管理命令只允许当前群管理员、群主或配置的机器人管理员。每个目标必须仍是当前群成员，机器人必须具有实际管理权限；机器人管理员不会绕过 QQ 侧的角色限制。一次最多处理 32 个目标，最多四个并发。成员的 `shut_up_end_time` 为 Milky 可选字段：无法确认列表时明确提示，可用真实 @ 直接解禁；长列表按消息预算展示，其余成员也可直接 @ 操作。

每次轮盘命中及每个管理员操作目标都有独立提交记录。提交前检查当前机器人账号、来源消息、插件版本、会话开关和角色权限；连接中断或取消后的未知结果不自动重试，重启也不重放旧禁言或解禁。

## 结构化消息

使用标准 Milky `text/mention/mention_all/face/reply/image/record/video/forward`；不开放任意 JSON API 或小程序消息。一次最多 128 个消息段、32 个转发节点、三层转发嵌套、32768 字节文本和 256 KiB 总消息。媒体 URI 仅允许无凭据与片段的有界 HTTP(S) URL；本地生成媒体先交给有容量和有效期限制的资源服务。

戳事件保留原始事件，使用负数哈希指纹做内部去重，与真实非负消息序号分离；不能拿来引用消息，也不进入 AI 对话或记忆。真实消息原始结构保留给桥接使用，AI 仍使用原有规范化文本。
