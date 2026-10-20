# 原生插件数据来源

行为与来源说明见 [native-reference.md](../../docs/native-reference.md)。以下文件仅作本地数据，不执行任何外部代码。

| 目录 | 来源及固定版本 | 处理方式 |
| --- | --- | --- |
| `wordle/*.json` | `noneplugin/nonebot-plugin-wordle`，`d6d73494c943c519e28344bad5e929f07502fb2f` | 保留十份词库中 3–8 字母 ASCII 单词与中文释义（大小写归一、重名释义合并），共 38,338 项；原始文件哈希见 `sources.json` |
| `wordle/legal.txt` | PyPI `pyspellchecker` 0.8.1 wheel，`spellchecker/resources/en.json.gz` | 保留 3–8 字母小写 ASCII 词并排序，共 55,222 个；wheel 哈希见 `sources.json` |
| `kfc/post.json` | `MinatoAquaCrews/nonebot_plugin_crazy_thursday`，`4734a3a2682d60e8643b4caaed8cd90e8eb664ac` | 原始 196 条文案，未改写 |
| `meal/menu.json`、`meal/images/*` | `Cvandia/nonebot-plugin-whateat-pic`，`df38a4fb3452a9fd8752c71a7af0c6979e894d48` | 保留 66 张菜品、85 张饮品原图，共 11,275,270 字节；以 SHA256 命名，清单保留原名称、URL 和校验值，不包含上游字体 |

各目录保留相应 MIT 许可证。`LICENSE-pyspellchecker` 属于合法猜测词表。没有导入 deer 插件的 AGPL 实现或任何图像素材。

餐图直接来自上述提交的 `res/eat_pic`、`res/drink_pic`，未修改图片内容；保留上游仓库声明，不为各图片的第三方来源作额外授权声明。归档及生成清单的哈希记录于 `sources.json`，每张图片的 SHA256 记录于 `meal/menu.json`。运行时按需读取原图并通过本机媒体地址发送，不需要访问 GitHub。
