# Podman 部署、升级与备份

本项目提供 Rust 镜像、可选 NoneBot 镜像、简单的 Podman 脚本和可选 Quadlet 单元。不依赖 compose，不把 QQ 客户端、Python 或浏览器塞进 Rust 镜像。

## 1. 准备 Linux 主机

Debian 13 示例：

```bash
sudo apt-get update
sudo apt-get install -y podman uidmap slirp4netns fuse-overlayfs git python3
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

固定餐图库随镜像放在 `/usr/share/qkbot/meal-images`，由 `QKBOT_MEAL_IMAGES_DIR` 指定；151 张原图约 10.75 MiB，按需读取，不在启动时从 GitHub 下载。已有数据库菜单会自动映射到对应原图，无需重建数据库；管理员添加、删除的菜单继续保留。图片通过内部 `3215` 媒体地址发送，LLBot 必须能访问该回环地址。构建后的容器冒烟测试会在禁用网络的容器内校验全部餐图，漏打包或内容损坏将使测试失败。

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

### 4.1 部署独立 LLBot

本次核对的稳定版为 **8.3.2**，官方安装器使用 `linyuchen/llbot` 镜像仓库；这里固定为 `docker.io/linyuchen/llbot:8.3.2`，不使用滚动 `latest`。[官方安装器](https://github.com/LLOneBot/LuckyLilliaBot/blob/dc2b2b90092ef0f571b5546878288b83fff1e436/script/install-llbot-docker.sh)、[官方安装说明](https://luckylillia.com/guide/choice_install)

```bash
podman pull docker.io/linyuchen/llbot:8.3.2
```

腾讯云内网无法直接拉取 Docker Hub 时，可使用其内网缓存，再标记回 Quadlet 使用的官方镜像名：

```bash
podman pull mirror.ccs.tencentyun.com/linyuchen/llbot:8.3.2
podman tag mirror.ccs.tencentyun.com/linyuchen/llbot:8.3.2 \
  docker.io/linyuchen/llbot:8.3.2
```

缓存仅适用于腾讯云内网；保留明确版本和 TLS 校验。[腾讯云镜像加速说明](https://cloud.tencent.com/document/product/213/8623)

本方案采用 **Linux host 网络和 LLBot 直连模式**，工作目录为 `/app/llbot`，配置与登录数据全部放在宿主 `~/workspace/llbot`：

| 宿主文件或目录 | 用途 |
| --- | --- |
| `default_config.json` | 首次登录的配置模板，只读挂载给容器 |
| `data/` | 账号配置、会话、机器标识、授权及协议端数据，整体持久化 |
| `data/webui_token.txt` | LLBot WebUI 原始密码，对应小鱼的“QQ 登录管理 / WebUI 密码” |
| `data/milky_token.txt` | 本部署保存的 Milky 令牌副本；LLBot 实际从配置的 `milky.http.accessToken` 读取 |
| `data/auth_token.txt` | LLBot 签名授权凭据，与 WebUI 密码、Milky 令牌均不同 |
| `data/auto_login_qq` | 可选的自动恢复账号选择，填写已扫码成功的本人 QQ 号 |
| `data/tmp/` | 原生签名模块装载临时共享库所需的可写目录 |
| `deploy/llbot.container`、`deploy/entrypoint.sh`、`deploy/container.marker` | 本地 Quadlet 配置、启动脚本及容器识别标记 |
| `backups/` | 停机备份 |

下面是**新安装**的初始化方法，需要 Python 3 作为一次性配置工具；它不参与机器人运行。脚本从固定版本镜像读取完整默认模板，生成两份独立随机凭据，拒绝覆盖已有配置或登录数据：

```bash
umask 077
python3 - <<'PY'
import json
import secrets
import subprocess
from pathlib import Path

base = Path.home() / "workspace" / "llbot"
data = base / "data"
if (base / "default_config.json").exists() or (data.exists() and any(data.iterdir())):
    raise SystemExit("已有 LLBot 配置或数据，请保留并检查，不执行首次初始化。")
template = subprocess.check_output([
    "podman", "run", "--rm", "--entrypoint", "/bin/cat",
    "docker.io/linyuchen/llbot:8.3.2", "/app/llbot/default_config.json",
])
config = json.loads(template)
webui_token, milky_token = secrets.token_hex(32), secrets.token_hex(32)
config["webui"].update(enable=True, host="127.0.0.1", port=3080)
config["milky"]["enable"] = True
config["milky"]["http"].update(host="127.0.0.1", port=3010, prefix="", accessToken=milky_token)
config["satori"]["enable"] = False
config["ob11"]["enable"] = False
config["ffmpeg"] = "/usr/bin/ffmpeg"
for directory in (base, data, data / "tmp", base / "deploy", base / "backups"):
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    directory.chmod(0o700)
for destination, content in (
    (base / "default_config.json", json.dumps(config, ensure_ascii=False, indent=2) + "\n"),
    (data / "webui_token.txt", webui_token),
    (data / "milky_token.txt", milky_token),
):
    with destination.open("x", encoding="utf-8") as file:
        file.write(content)
    destination.chmod(0o600)
print("LLBot 配置已生成；凭据保存在私有 data 目录，不输出到日志。")
PY
```

这里按 v8.3.2 的分协议 `host` 字段设置回环监听；不要套用旧文档的 `onlyLocalhost`。WebUI 密码从 `data/webui_token.txt` 读取，不是 `webui.password`；关闭 Satori 使用 `satori.enable`。[该版本默认配置](https://github.com/LLOneBot/LuckyLilliaBot/blob/dc2b2b90092ef0f571b5546878288b83fff1e436/src/main/config/default_config.json)

签名授权另按 LLBot 官方流程获取。首次安装可在服务器交互终端写入；已有有效文件时保留它：

```bash
cd ~/workspace/llbot
umask 077
if [ ! -s data/auth_token.txt ]; then
  read -r -s -p 'LLBot Auth Token: ' llbot_auth_token
  printf '\n'
  test -n "$llbot_auth_token" || { echo '未输入授权凭据。'; exit 1; }
  printf '%s' "$llbot_auth_token" > data/auth_token.txt
  unset llbot_auth_token
fi
```

该值来自 [LLBot 授权页面](https://auth.luckylillia.com)，不能用随机生成的 WebUI 或 Milky 令牌替代。小鱼管理员密码是另一套凭据，不应填进这三个文件。

复制并连接本项目的 LLBot Quadlet。所有可变部署配置仍位于 `~/workspace/llbot`；已有同名文件或链接时先检查，不强制覆盖：

```bash
mkdir -p ~/workspace/llbot/deploy ~/.config/containers/systemd
cp -n ~/workspace/qkbot-r/deploy/llbot.container \
  ~/workspace/llbot/deploy/llbot.container
cp -n ~/workspace/qkbot-r/deploy/llbot-entrypoint.sh \
  ~/workspace/llbot/deploy/entrypoint.sh
chmod 600 ~/workspace/llbot/deploy/entrypoint.sh
touch ~/workspace/llbot/deploy/container.marker
chmod 600 ~/workspace/llbot/deploy/container.marker
ln -s "$HOME/workspace/llbot/deploy/llbot.container" \
  "$HOME/.config/containers/systemd/llbot.container"
systemctl --user daemon-reload
systemctl --user start llbot.service
systemctl --user is-active llbot.service
```

单元通过 `/bin/sh` 运行只读挂载的本项目启动脚本，默认 `exec node --enable-source-maps ./llbot.js`，保持 LLBot CLI 默认登录行为，并只读挂载配置模板。官方 `docker/startup.sh` 会把 `webui.host` 从 `127.0.0.1` 改为空，本方案避免该改写，保持 3080 只监听回环；Milky 的 3010 也明确设为回环，不需要 `-p` 或防火墙开放协议端口。[本项目启动脚本](../deploy/llbot-entrypoint.sh)、[官方固定版本启动脚本](https://github.com/LLOneBot/LuckyLilliaBot/blob/dc2b2b90092ef0f571b5546878288b83fff1e436/docker/startup.sh)

空 `container.marker` 以只读方式挂载到 `/.dockerenv`。该版本只用这个路径判断 Docker 环境，Podman 需显式提供，才能使用预期的容器内机器标识持久化路径。保留整个 `data/`，包括 `machine_guid.bin` 和账号会话，不要仅搬运账号 JSON 或在换容器时重新生成标识。[容器识别源码](https://github.com/LLOneBot/LuckyLilliaBot/blob/dc2b2b90092ef0f571b5546878288b83fff1e436/src/common/utils/environment.ts)

首次登录会从 `default_config.json` 创建账号自己的 `data/config_<QQ号>.json`。已有账号使用其账号配置，修改模板不会自动覆盖它；检查实际账号配置的监听地址和 Milky 令牌。[官方配置说明](https://luckylillia.com/guide/config)

登录后检查监听地址，3080 与启用后的 3010 都应是 `127.0.0.1`，不能出现 `0.0.0.0` 或 `[::]`：

```bash
ss -ltn '( sport = :3080 or sport = :3010 )'
```

升级、迁移或备份 LLBot 时先停止它，保存完整数据与部署配置，再启动：

```bash
systemctl --user stop llbot.service
umask 077
mkdir -p "$HOME/workspace/llbot/backups"
tar -C "$HOME/workspace/llbot" -czf \
  "$HOME/workspace/llbot/backups/llbot-$(date +%Y%m%d-%H%M%S).tar.gz" \
  data default_config.json deploy
systemctl --user start llbot.service
```

备份包含授权、私有令牌和登录态，保存在权限受限的 `backups/` 中。镜像升级还应显式修改单元中的固定版本，拉取对应镜像后 `daemon-reload` 并重启，不依赖自动更新标签。仓库更新不会自动替换 `~/workspace/llbot/deploy/` 中已部署的单元和启动脚本；更新时先核对差异，再停机替换对应文件。

恢复时先停止 `llbot.service`，将当前 `data/`、`default_config.json` 和 `deploy/` 移到 workspace 内另一个备份目录保留，再把归档中的这三项完整解压回 `~/workspace/llbot/`。核对目录为 `700`、凭据和配置文件为 `600`，确认用户 Quadlet 链接仍指向恢复后的 `deploy/llbot.container`，拉取单元指定的镜像后执行 `systemctl --user daemon-reload` 和 `systemctl --user start llbot.service`。不要混合两份不同时间的机器标识与账号会话；恢复后通过管理页核对在线状态，失效会话需重新扫码。

### 4.2 通过小鱼管理页扫码

在自己的电脑建立转发，使用 13210 避免与本机服务冲突：

```bash
ssh -N -L 127.0.0.1:13210:127.0.0.1:3210 你的SSH主机别名
```

打开 http://127.0.0.1:13210：

1. 用 `podman logs qkbot` 中的一次性初始化码设置管理员密码。
2. 配置 Milky 地址与 token；LLBot 登录设置填写 WebUI 地址与 WebUI 密码。
3. 获取 QQ 二维码并扫码确认。
4. 填写模型地址与密钥，获取模型列表或手填模型。
5. 选择人设、插件与工具，点击“保存并应用”。
6. 核对运行状态、账号与实际加载的插件，再用自己的 QQ 测试。

按上节部署后，Milky 地址填写 `http://127.0.0.1:3010`，访问令牌使用 `data/milky_token.txt` 的值；QQ 登录管理地址填写 `http://127.0.0.1:3080`，WebUI 密码使用 `data/webui_token.txt` 的值。先保存这些接入参数，再在 QQ 登录页获取二维码并由账号本人扫码确认。签名授权 `auth_token.txt` 留在 LLBot 数据目录，不填入这两项。

扫码由小鱼服务端代理 LLBot WebUI，电脑只需转发 3210；无须把 3080 或 3010 开放到公网，也不必把它们另行转发。LLBot 需按 [官方文档](https://luckylillia.com/guide/config) 单独安装与授权。Rust 镜像不包含 QQ、签名服务或授权；协议端未就绪时管理页仍可初始化。不要让新旧两份小鱼同时连接同一账号，以免重复响应。

扫码成功后，如需下次启动自动恢复这个账号，可在服务器保存**本人刚刚登录成功的 QQ 号**：

```bash
cd ~/workspace/llbot
umask 077
read -r -p '已扫码成功的本人 QQ 号（仅数字）：' llbot_qq
printf '%s\n' "$llbot_qq" > data/auto_login_qq
chmod 600 data/auto_login_qq
unset llbot_qq
```

文件必须是 1–20 位数字；启动脚本遇到空值或非法字符会拒绝启动。存在有效文件时，脚本给 LLBot 加上 `-q <QQ号>`，使用持久化的会话尝试恢复；写入文件不会打断当前已登录实例，下次重启才读取它。没有该文件时保持 LLBot 原生默认行为，重启后通过小鱼重新扫码，或在 LLBot 原 WebUI 选择快速登录。该文件只选择账号，不替代 `data/` 登录态和签名授权；会话失效后仍需重新扫码。

2026-10-20 实机已验证：用户完成扫码后，小鱼管理页显示 QQ 已登录，运行状态为 `running`，Milky 返回的登录账号与实际账号一致。保留完整 `data/` 和 `auto_login_qq`、重新创建 LLBot 容器后，签名授权仍有效，账号自动恢复在线，Milky 继续只监听 `127.0.0.1:3010`，用户服务没有异常重启。同日从宿主使用已保存的模型配置完成两次合成请求，验证了工具调用、回传工具结果和最终文本回复；两次均返回 HTTP 200。该检查没有向 QQ 发送消息，完整的 QQ 聊天链路仍需由账号本人试聊确认。

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

### 9.1 历史记忆重整

升级到支持此命令的版本后，可为仍保留的历史原文创建一次可续跑的补整理任务。先短暂停止小鱼主进程，按本节步骤完整备份配置和数据库所在数据卷，再恢复服务；Quadlet 使用 `systemctl --user stop qkbot.service` 后直接导出 `qkbot-data`，备份完成后执行 `systemctl --user start qkbot.service`。备份包含凭据和历史，使用 `umask 077` 创建并妥善保管。回扫期间小鱼可以继续服务，LLBot／Milky 也须保持在线，命令需要核验当前机器人账号。

原生部署使用相同配置和数据库路径运行：

```bash
# 创建任务并冻结来源窗口，不调用模型。
qkbot --config /path/to/config.toml --rebuild-memory daily-memory-20261021 --rebuild-max-batches 0

# 每次最多处理 20 批；再次执行同一命令会从已提交进度续跑。
qkbot --config /path/to/config.toml --rebuild-memory daily-memory-20261021 --rebuild-max-batches 20

# 查询任务统计，不调用模型。
qkbot --config /path/to/config.toml --memory-rebuild-status daily-memory-20261021
```

`TASK` 为 1–64 个 ASCII 字母、数字、`-`、`_` 或 `.`。`--rebuild-max-batches N` 限制本次运行的处理批数，默认 `1000`，允许 `0–10000`。`0` 会核验 Milky 账号、写入任务记录并冻结窗口，不是只读预览。任务名与首次核验的账号、会话边界和来源上限绑定；同名续跑不扩大范围，也不重复提交已完成批次。状态包含已完成子任务、处理批数、来源数、新增数、重复数和已过期候选数。

容器部署推荐在完成备份并恢复服务后，直接在当前容器执行维护命令，复用已经部署的程序、配置、凭据和网络：

```bash
podman exec qkbot /usr/local/bin/qkbot --config /data/config.toml \
  --rebuild-memory daily-memory-20261021 --rebuild-max-batches 0

podman exec qkbot /usr/local/bin/qkbot --config /data/config.toml \
  --rebuild-memory daily-memory-20261021 --rebuild-max-batches 20

podman exec qkbot /usr/local/bin/qkbot --config /data/config.toml \
  --memory-rebuild-status daily-memory-20261021
```

容器名称或配置路径不同时替换为实际值。维护命令通过事务检查来源和记忆版本；正常后台整理并发更新时，当前批次不会以旧快照提交，会重新读取后再尝试。无需为了整个回扫过程保持停机，也不要并行执行多个同名回扫命令。

任务只选取当前账号、当前会话版本、当前有效聊天范围内，未暂停且允许记忆写入的会话。来源必须是当时已标记可学习的真人原文，并继续通过撤回、遗忘和人物归属检查。首次冻结的窗口包括符合条件的已处理及未处理原文；冻结的是范围，并不额外保存原文副本。已过保留期被清理的消息、旧会话版本及当时不可学习的消息无法补回。会话边界、权限或配置发生变化时，检查可能中止本次执行，应先核对原因；额度暂时耗尽或请求失败时，已提交进度保留，可稍后使用同名任务续跑。

重整只插入缺失项。模型会只读参考同一会话、同一人物的全部既有记忆，包括人工、锁定、停用和过期记录；不修改或删除旧记忆，不恢复停用或过期项。临时记忆按证据原消息时间计算有效期，已到期候选跳过。人物背景、兴趣习惯及交流偏好须有本人原文；“聊天观察”须有本人两条不同消息的证据，并设置 7–30 天有效期。记忆容量限制仍生效，不通过淘汰旧记忆为回扫腾出空间。

维护命令不启动 `Host`，不启动 QQ 消息处理或管理服务，不修改发送账本（outbox）、人设、摘要或正常记忆整理游标。它会写入自己的任务进度、新增记忆和模型用量；模型调用与日常后台整理共用现有每小时额度。当前回扫输入预算为 `32768`，输出预算读取 `[plugins.ai].memory_output_budget`（默认 `3200`），不会将预算或其他参数写回配置。原文和完整去重参考放不进预算时会停止，不通过截断既有记忆绕过去重。

## 10. 从原生部署迁移

1. 正常停止原进程，备份配置、SQLite 和管理员凭据。
2. 在 workspace 的独立目录整理 `config.toml`、数据库、同目录 `.qkbot-admin/` 及插件资源。
3. Windows 绝对路径改为容器 `/data` 内路径或相对路径。字体使用镜像提供的 Noto Sans CJK。
4. 核对 Milky／LLBot 地址与网络拓扑。跨机器不能继续使用原机器的 `127.0.0.1`。
5. 导入新命名卷，验证 UID 10001 读写权限，先运行容器 `--check`。
6. 启动并核对管理员、黑名单、记忆及插件状态，确认前保留旧数据。

不要同步正在运行的数据库，不要把生产配置上传 Git。基准测试使用独立配置、端口和数据库；真实样本见 [性能报告](performance.md)。
