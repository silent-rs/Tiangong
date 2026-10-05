# 远程访问（手机 H5）

手机浏览器扫码即可使用天工的对话功能。支持两种模式：

- **局域网直连（默认）**：不需要部署任何服务。桌面端在本机监听一个端口（默认 `8790`），手机和电脑在同一局域网内时扫码直接访问。
- **中继服务**：部署 `tiangong-relay` 后，手机可以在任意网络下经中继访问。中继是纯转发服务，**部署时不需要配置任何令牌**。

## 架构

```
局域网直连：手机浏览器 ──HTTP/WS──▶ 桌面端内嵌中继（0.0.0.0:8790）
中继服务：  手机浏览器 ──HTTPS/WSS──▶ tiangong-relay ◀──WSS（桌面端主动连接）── 天工桌面端
```

- **通道密钥由天工生成**：第一次启用时，天工自动生成一个随机的通道密钥，只保存在本机 `~/.tiangong/remote.json` 里。桌面端带着这个密钥接入中继；中继只用它的单向摘要（通道 ID）把手机端请求路由到对应的桌面端。只知道通道 ID 的人没法冒充桌面端。
- **中继只转发**：页面资源、会话媒体和手机端消息都经隧道交给桌面端处理；中继不保存会话数据，也不做业务判断。一个中继可以同时服务多台天工桌面端，各通道之间的消息、资源和连接互相隔离。
- **两种模式用的是同一套代码**：局域网直连时，桌面端在本机启动一个内嵌的中继（只允许一个桌面端接入），再通过回环地址接入它。配对、命令白名单、单设备限制在两种模式下完全相同。
- **处理逻辑和本地完全一致**：手机端用的是同一份前端构建产物，命令通过主窗口的 IPC 入口分发，命令表、状态和事件跟桌面端是同一套。
- **只开放对话功能**：桌面端按白名单判断每条命令（`src-tauri/src/remote/policy.rs`）。设置、插件管理、拓展区、浏览器控制、本地文件选择都会被拒绝；插件只开放挂在对话侧挂载点上的那部分。
- **单设备**：扫码时带一个一次性配对码（10 分钟内有效，只能用一次）。配对成功后桌面端签发设备令牌，本地只保存它的 SHA-256 摘要。重新扫码会让旧设备失效；同一设备同时只允许一条连接在线，新连接上来时旧连接会被踢下线。
- **单桌面端**：每个通道同时只接受一个桌面端接入，后来的会被拒绝。

访问地址的格式是 `<地址>/?c=<通道 ID>`。首次打开后，中继会用 Cookie 记住通道，页面里的静态资源请求靠它来路由。

## 局域网直连

1. 打开「设置 → 远程访问」，选择「局域网直连」，然后启用。
2. 「局域网地址」留空时会自动探测本机的私有 IPv4 地址，优先级为 `192.168.x.x` > `10.x.x.x` > `172.16–31.x.x`。代理软件的虚拟网卡（如 `198.18.0.0/15`）、CGNAT 地址和公网地址不会被选中。探测结果不对时，手动填写手机能访问到的地址即可。
3. 端口默认 `8790`；被占用时状态会显示错误，换一个端口就行。
4. 状态显示已连接后，点「生成二维码」，用手机扫码。

注意：

- 首次启用时，系统防火墙可能会询问是否允许天工接受入站连接，需要选择允许。
- 局域网直连走明文 HTTP，只适合在可信网络（家里、办公室）里使用。公共 Wi-Fi 等环境请改用中继服务，并在中继前面配上 HTTPS。
- 电脑的 IP 变了（比如换了网络）以后，需要重新生成二维码并扫码。设备令牌保存在手机浏览器里，按访问地址区分，换了地址就读不到原来的令牌。

## 部署中继（Linux）

### 一键安装（systemd 守护运行）

```bash
curl -fsSL https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/relay/install.sh | sudo bash
```

脚本会做这些事：

1. 默认从阿里云 OSS 读取最新版本（`relay/latest.json`），按 CPU 架构（x86_64 / aarch64）下载静态二进制，并校验 SHA-256；海外服务器可以设置 `TIANGONG_RELAY_SOURCE=github`，改从 GitHub Release 下载；
2. 安装到 `/usr/local/bin/tiangong-relay`；
3. 创建系统用户 `tiangong-relay`；
4. 写入配置 `/etc/tiangong-relay/relay.env` 和 systemd 服务 `tiangong-relay.service`（带安全加固），然后启动服务并做健康检查。

常用操作：

```bash
sudo bash install.sh install --version relay-v0.1.0 --listen 127.0.0.1:8790   # 指定版本与监听地址
sudo bash install.sh upgrade              # 升级到最新版；新版本起不来时自动回滚
sudo bash install.sh uninstall [--purge]  # 卸载（--purge 同时删除配置与系统用户）
bash install.sh status                    # 查看版本、服务状态与健康检查
systemctl restart tiangong-relay          # 修改 relay.env 后重启
journalctl -u tiangong-relay -f           # 查看日志
```

内网环境可以设置 `TIANGONG_RELAY_BASE_URL=<镜像前缀>`，并配合 `--version` 从镜像下载，目录结构和 OSS 一致：`<前缀>/<tag>/<制品>`。没有 systemd 的系统只会安装二进制，需要自行托管进程。

### 配置

| 参数 | 环境变量 | 说明 |
|------|----------|------|
| `--listen` | `TIANGONG_RELAY_LISTEN` | 监听地址，默认 `0.0.0.0:8790` |
| `--max-agents` | `TIANGONG_RELAY_MAX_AGENTS` | 最多同时接入的天工桌面端数量，默认 256 |
| `--tls-cert` / `--tls-key` | `TIANGONG_RELAY_TLS_CERT` / `TIANGONG_RELAY_TLS_KEY` | PEM 证书与私钥，两者同时提供时中继直接以 HTTPS 监听（不使用反向代理时） |

本地联调可以用 mkcert 签发 `localhost` 证书，以 `https://localhost:8443` 启动中继：`bash scripts/relay/dev-local.sh`。

中继不需要令牌。从源码运行：`cargo run --release -p tiangong-relay -- --listen 127.0.0.1:8790`。

公网部署时，建议把中继监听在 `127.0.0.1`，前面放一个 HTTPS 反向代理，并开启 WebSocket 升级。nginx 示例：

```nginx
location / {
    proxy_pass http://127.0.0.1:8790;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 1h;
    client_max_body_size 128m;
}
```

健康检查：`GET /healthz` 返回 `{"ok":true,"version":"…","agents":<在线桌面端数>}`；带上 `?c=<通道 ID>` 时还会返回 `agent_online`。

### 发布

- 中继单独发版，标签格式为 `relay-v<version>`，版本号要和 `crates/tiangong-relay/Cargo.toml` 一致。推送标签后，`.github/workflows/release-relay.yml` 会：
  - 构建 x86_64 和 aarch64 的 musl 静态二进制，生成 `.sha256`，连同 `install.sh` 一起挂到 GitHub Release；
  - 同步上传到阿里云 OSS（`oss://silent-tiangong/relay/`）：版本目录 `relay/<tag>/`、最新版本指针 `relay/latest.json`、固定安装入口 `relay/install.sh`。草稿发布只上传版本目录，不切换 latest。上传后会校验 `latest.json` 和二进制是否能下载。
- `.github/workflows/relay-ci.yml` 在 PR 中做这些检查：运行测试、构建 musl 静态二进制、启动二进制做冒烟测试（健康检查），以及对安装脚本跑 ShellCheck。

## 中继模式使用

1. 打开「设置 → 远程访问」，选择「中继服务」，填写中继地址（如 `https://relay.example.com`），然后启用。不需要填令牌。
2. 状态显示「已连接」后，点「生成二维码」，用手机浏览器扫码打开。
3. 配对成功后二维码自动收起。之后在手机上打开同一个访问地址即可，不用再扫码。
4. 要更换设备，重新扫码即可；要停止远程使用，点「解除绑定」或关闭开关。
5. 通道地址泄露，或者提示「该通道已有其他天工桌面端接入」时，点「远程通道 → 重置」换一个新通道。旧地址会立即失效，需要重新扫码。

两种模式切换后，需要重新扫码配对。配置保存在 `~/.tiangong/remote.json`（权限 0600）。

## 远程模式下的界面

- 右上角的搜索、插件管理、拓展区、主题切换按钮不显示；侧栏不显示设置入口。
- 手机上点「新对话」或切换会话后，侧栏会自动收起。
- 页面高度跟随可视区域，弹出软键盘时输入框和发送按钮仍然可见；页面禁用缩放，输入框字号为 16px，避免 iOS 聚焦时自动放大。
- 附件只能从手机上传，不能引用桌面端的本地路径；不能修改对话目录。
- 消息中的链接在手机浏览器新标签页打开。
- 会话中的本地媒体文件只能读取桌面端 asset 协议允许的目录（`~/.tiangong`、`~/Documents`、`~/Downloads`、`/tmp`），单个文件不超过 50MB，并以 `Content-Security-Policy: sandbox` 返回。访问密钥随连接签发，设备下线后即失效。
