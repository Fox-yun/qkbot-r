# 原生插件对照记录

核对日期：2026-10-20。用户随后提供完整仓库清单，确认下列四个项目为目标来源；原机器人实际安装的发行版本仍未知。以下固定版本作为可重现的兼容基准，不声称像素级复刻。普通签到、个人五字母小词库、纯文字餐单草稿已被替换。

| 功能 | 参考 PyPI 包及版本 | 固定源码提交 | 上游声明许可证 |
| --- | --- | --- | --- |
| 🦌管签到 | `nonebot-plugin-deer-pipe` 0.6.4 | `2e96e6857271800fa7ced7340134f3374bb1dd83` | AGPL-3.0-or-later |
| Wordle | `nonebot-plugin-wordle` 0.4.5 | `d6d73494c943c519e28344bad5e929f07502fb2f` | MIT |
| 疯狂星期四 | `nonebot-plugin-crazy-thursday` 0.2.8 | `4734a3a2682d60e8643b4caaed8cd90e8eb664ac` | MIT |
| 今天吃什么 | `nonebot-plugin-whateat-pic` 1.5.9 | `df38a4fb3452a9fd8752c71a7af0c6979e894d48` | MIT |

版本来源：[deer PyPI](https://pypi.org/project/nonebot-plugin-deer-pipe/0.6.4/)、[Wordle PyPI](https://pypi.org/project/nonebot-plugin-wordle/0.4.5/)、[星期四 PyPI](https://pypi.org/project/nonebot-plugin-crazy-thursday/0.2.8/)、[图片餐单 PyPI](https://pypi.org/project/nonebot-plugin-whateat-pic/1.5.9/)。已比对各仓库项目版本号；未把发行 wheel 与 Git 工作树逐文件比较。公开文本源码只读缓存于忽略目录 `target/native-upstream-review`，没有安装或执行上游 Python。

## 🦌管签到

`🦌` / `鹿` 可以给自己或群内被 @ 的成员记录，同一天重复操作会继续增加次数。`补🦌 日期` 只补当前月早于今天且尚未记录的日期。`🦌历 [@成员]` 输出当月日历图片，`🦌榜` 输出当前群本月前五名。[指令与处理源码](https://github.com/SamuNatsu/nonebot-plugin-deer-pipe/blob/2e96e6857271800fa7ced7340134f3374bb1dd83/src/nonebot_plugin_deer_pipe/matchers.py)

`帮🦌 on/off` 控制自己是否接受帮签，默认允许；指定他人需群管理员身份。`禁🦌 @成员 [时长]` 由群管理员设置，最多 30 天，省略时长解除限制。上游补签路径没有检查当前禁签时间，这是具体实现行为，不应误写为普通每日唯一签到。[权限与禁签源码](https://github.com/SamuNatsu/nonebot-plugin-deer-pipe/blob/2e96e6857271800fa7ced7340134f3374bb1dd83/src/nonebot_plugin_deer_pipe/matchers.py)

上游使用本地 `datetime.now()` 和 SQLModel；用户按适配器、场景、场景 ID、用户 ID 区分，记录有月份和日期；每周清理非当前月记录。Rust 实现应明确采用配置时区、带年份日期和 bot ID 隔离，避免跨年或跨账号混用，这属于有意修正而非复制存储缺陷。[数据库](https://github.com/SamuNatsu/nonebot-plugin-deer-pipe/blob/2e96e6857271800fa7ced7340134f3374bb1dd83/src/nonebot_plugin_deer_pipe/database.py)、[定时任务](https://github.com/SamuNatsu/nonebot-plugin-deer-pipe/blob/2e96e6857271800fa7ced7340134f3374bb1dd83/src/nonebot_plugin_deer_pipe/schedule.py)

此候选声明 AGPL-3.0-or-later。本项目保持独立 Rust 行为实现，不复制其实现、字体、图片或其他素材；渲染使用本项目独立实现。[许可证声明](https://github.com/SamuNatsu/nonebot-plugin-deer-pipe/blob/2e96e6857271800fa7ced7340134f3374bb1dd83/pyproject.toml)

验收范围：重复计数、帮签与本人开关、管理员修改、补签日期边界、限时禁签、月历、群前五、并发与重启、年月及账号隔离。

## Wordle

默认五字母，可用 `wordle -l 3..8 -d 词库` 调整；尝试次数是长度加一。群内成员共享一局，私聊各自独立。游戏中直接发送对应长度的英文字母串猜词；`wordle_hint` / `提示` / `猜单词提示` 获取提示，`wordle_stop` / `结束` / `结束游戏` / `结束猜单词` 结束。有效命令前缀和 @ 条件由匹配器处理。每次匹配猜测或提示会重置 300 秒超时；上游在内存保存局面，重启不保留。[会话、匹配及计时](https://github.com/noneplugin/nonebot-plugin-wordle/blob/d6d73494c943c519e28344bad5e929f07502fb2f/nonebot_plugin_wordle/__init__.py)

答案从指定完整 JSON 词库选取，默认 CET4；包括 CET4、CET6、GRE、考研、GMAT、专四、TOEFL、SAT、专八、IELTS。**合法猜词由 pyspellchecker 英语词典判断，并不要求出现在答案词库中。** 因而几十个自建词或把答案库当全部合法词，都不兼容。[词库与合法性](https://github.com/noneplugin/nonebot-plugin-wordle/blob/d6d73494c943c519e28344bad5e929f07502fb2f/nonebot_plugin_wordle/utils.py)

绿格先消耗对应字母，黄格再消耗剩余次数；非法词和重复猜词不扣次数。提示展示答案中曾被猜到的字母所在位置，其余为星号；结束展示答案和中文释义。棋盘与提示是 PNG。[游戏规则与渲染](https://github.com/noneplugin/nonebot-plugin-wordle/blob/d6d73494c943c519e28344bad5e929f07502fb2f/nonebot_plugin_wordle/data_source.py)

上游代码采用 MIT；引入词表时保留来源、固定版本和许可证，字体不随代码许可证推定授权。[LICENSE](https://github.com/noneplugin/nonebot-plugin-wordle/blob/d6d73494c943c519e28344bad5e929f07502fb2f/LICENSE)

验收范围：群共享、3–8 字母、长度加一次数、真实答案库及独立合法猜测词典、重复字母、无效/重复猜词、提示、主动结束、300 秒续期及到期通知、中文释义、图片输出。持久化重启恢复如采用，需注明是本项目增强。

## 疯狂星期四

此候选用 `疯狂星期X` 和 `狂乱X曜日` 的整句正则触发：支持中文一至日/天及日文月火水木金土日。随机读取本地 `post.json` 文案，把中文、日文及英文星期替换成用户指定星期，**不限制实际星期四**。源码没有冷却、按用户状态或每周主动群发。[实现](https://github.com/MinatoAquaCrews/nonebot_plugin_crazy_thursday/blob/4734a3a2682d60e8643b4caaed8cd90e8eb664ac/nonebot_plugin_crazy_thursday/__init__.py)、[MIT 许可证](https://github.com/MinatoAquaCrews/nonebot_plugin_crazy_thursday/blob/4734a3a2682d60e8643b4caaed8cd90e8eb664ac/LICENSE)

另有 `nonebot-plugin-kfc-crazy` 等同名功能项目，可能包含定时群发；不能把它们的规则混入此候选。此前五段原创文案、当前星期前缀及 60 秒冷却不是本候选行为。

验收范围：完整触发形式、非法星期提示、随机本地文案、三种语言星期替换；资源保留上游声明，禁止默认新增冷却或定时群发来冒充兼容。

## 今天吃什么

此候选的核心是“今天吃什么”“今天喝什么”分别随机发送菜品/饮品名称和**真实图片**，不排除连续重复。全部菜单/查看菜单/查看菜品按类型生成菜单图片；添加菜单需要名称、类型和图片，删除菜单需要名称和类型。源码增删权限为 **NoneBot SUPERUSER**，并非仅看群主或管理员称号。[命令与权限](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/nonebot_plugin_whateat_pic/matcher.py)、[随机图片](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/nonebot_plugin_whateat_pic/random_pic.py)、[菜单渲染](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/nonebot_plugin_whateat_pic/menu.py)

默认冷却 10 秒、每日次数上限 0（不限）。其冷却是进程共享；吃喝次数按用户 ID 共用，早上八点重置，内存状态重启重置。启动时会检查并下载缺失的资源；“离线”不意味着首次启动无需资源准备。[配置](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/nonebot_plugin_whateat_pic/config.py)、[调用与状态](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/nonebot_plugin_whateat_pic/matcher.py)、[项目说明](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/README.md)

源码声明 MIT；图片、字体等素材需逐项确认来源，不能仅凭根目录 LICENSE 推定所有第三方素材均可再次分发。[LICENSE](https://github.com/Cvandia/nonebot-plugin-whateat-pic/blob/df38a4fb3452a9fd8752c71a7af0c6979e894d48/LICENSE)

验收范围：吃/喝分库、图片发送、菜单图片、增删与明确的管理员权限、冷却与次数配置、资源缺失可诊断。原文字餐单及避免连续重复策略不能代替本功能。

## 本项目实现与资源边界

`src/plugins/native.rs` 使用共同 SQLite、现有账号/会话/插件开关及消息来源校验；同一来源的业务结果在事务内保存，重放不会再次累加签到或次数。所有四项均不调用模型。

- 🦌签到独立实现，不使用上游 AGPL 源码或素材。保留重复计数、帮签、补签、开关、管理员限时禁签、月历及前五名；采用配置时区和完整年月日，不按上游清理方式删除过去月份。昵称只用于展示，身份仍是账号、场景、会话、QQ ID。
- Wordle 引入十份真实答案词库，保留全部 3–8 字母的 ASCII 英文词及中文释义（大小写归一、重名释义合并），共 38,338 个词库项；合法猜词表来自 MIT 的 `pyspellchecker` 0.8.1，含 55,222 个对应长度英文词。局面持久化是本项目增强；每次匹配猜测及提示续期 300 秒。到期状态先原子终结，通知交给宿主发送账本，未知发送结果不自动重试。
- KFC 使用固定提交中的 196 条本地文案及其星期替换规则，不额外添加冷却和自动群发。
- 图片餐单附带上游固定提交的 151 张原图（66 张菜品、85 张饮品，共约 10.75 MiB），不引入上游字体。清单保留原 URL 作为稳定标识，兼容已存入数据库的菜单；发送时严格匹配清单并校验本地图片大小、格式和 SHA256，再通过受账号／会话／插件开关保护的本机媒体地址交给协议端，不再依赖运行时访问 GitHub。图片按需读取，不保留整套解码缓存；缺失或损坏会明确报错。管理员添加图片仍交给本机受控媒体库持久保存，打包图库不占自定义图片配额。增删仅允许 `plugins.native.superusers` 中的 QQ ID。
- 吃喝支持带或不带 `/` 的整句触发，包括 `早上吃什么`、`早餐吃什么`、`中午吃什么`、`午餐吃什么`、`晚上吃什么`、`晚饭吃什么`、`今晚吃什么`，并保留原有吃／喝与菜单管理入口。不在普通聊天正文中截取命令。各时段共用原菜品库；原版全进程共享状态改为每 bot 隔离，冷却中的请求不消耗每日次数，每日吃喝额度在配置时区的早上八点重置。菜单支持短时逐步补充名称、类型和图片，多个会话／用户互不占用。

数据位于 `assets/native`，保留 MIT 许可证。`sources.json` 记录原始文件哈希及词典 wheel 哈希；答案 JSON 仅做长度/字母过滤并保留中文释义，合法猜测词表从 wheel 数据文件提取，没有运行其 Python 实现。字体与图片绘制由本项目共享渲染器负责。
