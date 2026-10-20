# Podman 部署、升级与备份

本项目提供 Rust 镜像、可选 NoneBot 镜像、简单的 Podman 脚本和可选 Quadlet 单元。不依赖 compose，不把 QQ 客户端、Python 或浏览器塞进 Rust 镜像。

## 1. 准备 Linux 主机

Debian 13 示例：

```bash
sudo apt-get update
sudo apt-get install -y podman uidmap slirp4netns fuse-overlayfs git
sudo loginctl enable-linger "$USER"
podman info --format 'rootless={{.Host.Security.Rootless}} cgroup={{.Host.CgroupsVersion}}'
mkdir -p ~/workspace
cd ~/workspace
git clone https://github.com/Fox-yun/qkbot-r.git
cd qkbot-r
```

以普通用户运行后面的 Podman 命令，不要混用 `sudo podman`，否则会访问另一套镜像、容器与卷。`enable-linger` 用于退出 SSH 后保留用户服务；系统启动自启还需第 7 节的 Quadlet。

项目、部署记录和备份放在 workspace 中。镜像和命名卷由 Podman 管理，不是源码目录里的文件。默认编译并发为 1，适合小服务器；可以用 `BUILD_JOBS=2 sh deploy/podman.sh build` 调整。**编译峰值不代表运行占用**，运行层不包含编译器。

## 2. 选择网络拓扑

原生图片和 NoneBot 连接使用受约束的回环地址。Rust、LLBot 和 NoneBot 必须能够共同访问同一组 `127.0.0.1` 端口：

| 模式 | 适用情况 | 管理页监听 |
| --- | --- | --- |
| Linux host，脚本默认 | LLBot 在同一 Linux 主机原生运行，或也使用 host 网络 | `127.0.0.1:3210` |
| 同一 Pod | LLBot、Rust、NoneBot 都放入同一个 Pod | Pod 内 `0.0.0.0:3210`，只发布宿主 `127.0.0.1:3210` |

常用端口：Milky `3010`，LLBot WebUI `3080`，小鱼管理页 `3210`，内部媒体／NoneBot 入口 `3215`。实际值以协议端配置为准。**不要把 3215 暴露到公网，也不要用 `0.0.0.0` 替换 `nonebot.listen`。**它既是监听地址，也是协议端读取临时图片的地址。

Linux host 模式保持默认配置即可。脚本使用 `--network host`，不设置 `-p`，管理页默认只监听 `127.0.0.1:3210`。LLBot 在普通桥接容器中发布端口不等同于共享回环：回拉 `127.0.0.1:3215` 图片时会访问它自己的容器。应让 LLBot 也用 host 网络，或同 Pod：

```bash
podman pod create --name qkbot --publish 127.0.0.1:3210:3210
# 按 LLBot 官方镜像说明启动协议端，并添加 --pod qkbot。
# 同 Pod 的其他容器不要再单独设置 --publish 或 --network。
QKBOT_NETWORK=pod sh deploy/podman.sh start
```

LLBot 和可选 NoneBot 都加入 `qkbot` Pod，继续使用回环地址，内部端口无需发布。[Podman 网络共享说明](https://docs.podman.io/en/stable/markdown/podman-pod-create.1.html)

`QKBOT_NETWORK` 只控制新容器参数及**首次配置**，不会覆盖已有配置。切换模式时先停机备份，显式修改 `[web].listen`，再按第 8 节保留数据卷并重建容器；直接 `start` 已有容器不会改变其网络。切回 host 必须恢复回环监听，不要把 Pod 的 `0.0.0.0:3210` 直接带到 host 网络。

Windows/macOS 的 host 网络属于 Podman Linux 虚拟机，不能自动共享宿主 QQ 的回环地址。推荐整套服务位于同一 Linux 主机／Pod，或保留原生运行方式。[Podman 平台说明](https://podman.io/docs/installation)

## 3. 构建与启动

```bash
sh deploy/podman.sh build
sh deploy/podman.sh start
sh deploy/podman.sh status
sh deploy/podman.sh logs
```

| 项目 | 默认值 |
| --- | --- |
| 镜像／容器 | `localhost/qkbot-r:latest`／`qkbot` |
| 命名卷／配置 | `qkbot-data`／`/data/config.toml` |
| 管理凭据 | `/data/.qkbot-admin/credentials.json` |
| 默认数据库 | `/data/qkbot.sqlite3` |
| 停机等待 | 600 秒 |

支持 `QKBOT_IMAGE`、`QKBOT_CONTAINER`、`QKBOT_VOLUME`、`QKBOT_NETWORK`、`QKBOT_POD` 和 `QKBOT_STOP_TIMEOUT` 覆盖。每次操作使用同一组名称。已存在容器的 `start` 只是启动它，不会自动换新镜像、网络或停止时限，升级见第 8 节。

镜像内 UID/GID 为 `10001:10001`，运行时丢弃 capabilities，启用 `no-new-privileges` 和只读根文件系统；`/data` 卷及临时目录可写。新命名卷由 Podman 初始化权限。绑定已有目录时先处理 rootless UID 映射，不要把目录改成 777。

不要只挂载 `config.toml` 单文件：Web 保存需要同目录原子替换，管理员凭据也位于该目录。应持久化整个 `/data`。

如果 Docker Hub 不可达，使用自己信任的镜像缓存，保留版本与 TLS 验证。腾讯云官方内网加速源的构建示例：

```bash
podman build -f Containerfile -t localhost/qkbot-r:latest \
  --build-arg RUST_IMAGE=mirror.ccs.tencentyun.com/library/rust:1.99.1-bookworm \
  --build-arg RUNTIME_IMAGE=mirror.ccs.tencentyun.com/library/debian:bookworm-slim \
  --build-arg DEBIAN_MIRROR=http://mirrors.tencentyun.com \
  --build-arg CARGO_MIRROR=sparse+https://rsproxy.cn/index/ .
```

该加速源只适用于腾讯云内网，不是公共镜像源。[腾讯云说明](https://cloud.tencent.com/document/product/213/8623)

`CARGO_MIRROR` 可省略，默认直接使用 crates.io；示例采用字节跳动的 [RsProxy](https://rsproxy.cn/) 下载 Rust 依赖，仍使用 `Cargo.lock` 校验包哈希。`DEBIAN_MIRROR` 只替换容器构建中的包源地址，保留 Debian 签名校验。镜像源只作用于构建，不修改宿主全局配置。

## 4. 第一次访问和 QQ 登录

在自己的电脑建立转发，使用 13210 避免与本机服务冲突：

```bash
ssh -N -L 13210:127.0.0.1:3210 你的SSH主机别名
```

打开 http://127.0.0.1:13210：

1. 用 `podman logs qkbot` 中的一次性初始化码设置管理员密码。
2. 配置 Milky 地址与 token；LLBot 登录设置填写 WebUI 地址与 WebUI 密码。
3. 获取 QQ 二维码并扫码确认。
4. 填写模型地址与密钥，获取模型列表或手填模型。
5. 选择人设、插件与工具，点击“保存并应用”。
6. 核对运行状态、账号与实际加载的插件，再用自己的 QQ 测试。

LLBot 需按 [官方文档](https://luckylillia.com/guide/config) 单独安装与授权。本镜像不包含 QQ、签名服务或授权。协议端未就绪时管理页仍可初始化；不要让新旧两份小鱼同时连接同一账号，以免重复响应。

“保存当前配置”仅保存；“保存并应用”才启动新配置。原生插件不依赖 NoneBot。误开未配置独立令牌的 NoneBot 会阻止整份配置应用，旧实例仍使用旧设置。

## 5. 密钥与数据

Web 输入的密钥保存在私有数据卷中，不进入镜像。也可通过 Podman secret 注入，以下在服务器交互终端执行，避免密钥进入命令历史：

```bash
read -r -s -p '模型 API key: ' api_key
printf '%s' "$api_key" | podman secret create qkbot-api -
unset api_key
QKBOT_API_SECRET=qkbot-api sh deploy/podman.sh start
```

| 脚本变量，值为已有 secret 名 | 容器环境变量 |
| --- | --- |
| `QKBOT_API_SECRET` | `QKBOT_API_KEY` |
| `QKBOT_MILKY_SECRET` | `QKBOT_MILKY_TOKEN` |
| `QKBOT_LLBOT_SECRET` | `QKBOT_LLBOT_TOKEN` |
| `QKBOT_NONEBOT_SECRET` | `QKBOT_NONEBOT_TOKEN` |

在创建容器前传入；`start` 已存在容器不会增加环境变量或 secret。环境变量优先且不回写文件。Quadlet 使用 `[Container]` 的 `Secret=` 项。[Podman secret 参数](https://docs.podman.io/en/stable/markdown/podman-run.1.html#secret-secret-opt-opt)

`.containerignore` 白名单排除生产配置、数据库、日志、工具链与密钥。备份包含配置和管理员凭据，应保存在受限目录。

## 6. 可选 NoneBot

Rust 原生插件不需要 Python。有需要时再单独构建：

```bash
podman build -f sidecar/nonebot/Containerfile \
  --build-arg PROFILE=full \
  --build-arg WITH_BROWSER=1 --build-arg WITH_FFMPEG=1 --build-arg WITH_FONTS=1 \
  -t localhost/qkbot-nonebot:latest .
```

启动、独立 token、资源目录和功能差异见 [NoneBot 部署](../sidecar/nonebot/README.md#podman-部署可选)。core 不含六个上游功能；full 的浏览器／媒体依赖会增加镜像与运行成本，应单独测量。

## 7. 可选开机自启动

Linux 推荐 rootless Quadlet：

```bash
# 若已用脚本启动，先正常停止并删除容器外壳，数据卷保持不动。
sh deploy/podman.sh stop
podman rm qkbot
mkdir -p ~/.config/containers/systemd
ln -s "$HOME/workspace/qkbot-r/deploy/qkbot.container" \
  "$HOME/.config/containers/systemd/qkbot.container"
systemctl --user daemon-reload
systemctl --user start qkbot.service
systemctl --user status qkbot.service
```

链接使用 workspace 中的配置作为唯一来源；仓库位于其他位置时替换为它的绝对路径。目标已存在时先核对现有服务，不要加 `-f` 强制覆盖。修改链接指向的配置后执行 `systemctl --user daemon-reload`。

示例使用 Linux host 网络。同 Pod 需调整单元，不能混用。`[Install]` 已设置 `WantedBy=default.target`，不必对生成的服务执行 `systemctl enable`。linger 开启后可随用户管理器启动。

单元使用本地镜像，关闭了 Quadlet 默认的网络就绪等待，管理页可在网络或 QQ 尚未就绪时启动；小鱼会自行重试协议连接。部分主机的系统级 `network-online.target` 不会激活，保留默认等待会让每次启动多等 90 秒。这只关闭 Quadlet 的网络依赖，不关闭 systemd 的常规依赖。[Podman 5.4 网络等待说明](https://docs.podman.io/en/v5.4.2/markdown/podman-systemd.unit.5.html#implicit-network-dependencies)

后续使用 `systemctl --user stop/start/restart qkbot`，日志用 `journalctl --user -u qkbot`，不要再用脚本创建同名容器。停止等待为 600 秒，systemd 外层 630 秒；若自定义协议超时更大，应同步提高。[Quadlet 文档](https://docs.podman.io/en/stable/markdown/podman-systemd.unit.5.html)

## 8. 升级与回滚

脚本管理的容器先构建再停机：

```bash
cd ~/workspace/qkbot-r
git pull --ff-only
podman tag "$(podman inspect --format '{{.Image}}' qkbot)" localhost/qkbot-r:previous
sh deploy/podman.sh build
sh deploy/podman.sh stop
mkdir -p backups
sh deploy/podman.sh backup "backups/qkbot-$(date +%Y%m%d-%H%M%S).tar"
podman rm qkbot
sh deploy/podman.sh start
sh deploy/podman.sh status
```

`previous` 标记的是当前容器实际使用的镜像，避免 `latest` 已被其他构建移动时回滚到错误版本。不要给 `podman rm` 加 `--volumes`，不要删数据卷。重建时保留原 secret、名称与网络参数。已有管理员不应再次初始化，历史和插件状态也应保留。

Quadlet 模式通过 `systemctl --user stop qkbot` 停机，直接导出 `qkbot-data`，更新镜像后再 start。停机时 Quadlet 可能移除容器，此时应直接使用 `podman volume export`，不要依赖以容器名称定位卷的脚本。

数据库已迁移时，**只回退镜像不保证兼容**。回滚应停掉新版本、保留失败卷，将升级前备份恢复到新卷，再运行 `localhost/qkbot-r:previous`。不要让两个实例挂同一数据库卷。

## 9. 备份与恢复

SQLite WAL 写入时不要只复制 `.sqlite3`。停机导出整个卷：

```bash
sh deploy/podman.sh stop
mkdir -p backups
sh deploy/podman.sh backup backups/qkbot-before-change.tar
sh deploy/podman.sh start
```

脚本拒绝备份运行中的容器和覆盖已有备份。NoneBot 专属卷、Podman secrets、LLBot 配置与登录数据需要分别备份。

恢复到**新卷**：

```bash
podman volume create qkbot-restored
podman volume import qkbot-restored backups/qkbot-before-change.tar
# 旧实例必须先停止；不覆盖它的数据卷。
QKBOT_CONTAINER=qkbot-restored QKBOT_VOLUME=qkbot-restored sh deploy/podman.sh start
```

跨机器 UID 映射不同、或从普通目录导入时，先验证 UID 10001 可读写。保留备份，直到登录、历史和插件状态核对完毕。[卷导出文档](https://docs.podman.io/en/stable/markdown/podman-volume-export.1.html)

## 10. 从原生部署迁移

1. 正常停止原进程，备份配置、SQLite 和管理员凭据。
2. 在 workspace 的独立目录整理 `config.toml`、数据库、同目录 `.qkbot-admin/` 及插件资源。
3. Windows 绝对路径改为容器 `/data` 内路径或相对路径。字体使用镜像提供的 Noto Sans CJK。
4. 核对 Milky／LLBot 地址与网络拓扑。跨机器不能继续使用原机器的 `127.0.0.1`。
5. 导入新命名卷，验证 UID 10001 读写权限，先运行容器 `--check`。
6. 启动并核对管理员、黑名单、记忆及插件状态，确认前保留旧数据。

不要同步正在运行的数据库，不要把生产配置上传 Git。基准测试使用独立配置、端口和数据库；真实样本见 [性能报告](performance.md)。
