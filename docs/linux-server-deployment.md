# 天工 Linux 服务器部署指南

> 适用版本：0.12.0+
> 关联文档：`docs/rfc/0015-cli-modular-config.md`、`docs/server-api.md`、`README.md`

本文档面向在纯 Linux 服务器（无桌面环境、Docker 容器、远程主机）上以 Server 模式部署天工的场景。天工的同一个 `tiangong` 二进制同时支持桌面、CLI 和 Server 三种模式，本文聚焦 Server 模式。

---

## 1. 适用场景

- 无图形界面的 Linux 服务器（VPS、云主机、自建机房）。
- Docker / Podman 容器内运行。
- 希望通过 HTTP / WebSocket API 接入脚本、服务或外部消息通道。
- 需要长期常驻运行，由 systemd 或容器编排托管。

**关键说明**：天工的发布流水线（`.github/workflows/release.yml`）默认构建桌面安装包（macOS `.dmg`、Linux `.AppImage`、Windows NSIS）。这些安装包依赖 WebKit、GTK 等桌面运行时。**在纯服务器上，推荐通过源码编译获得无桌面依赖的纯二进制**（`target/release/tiangong`），它只包含 CLI/Server 能力，体积更小、依赖更少。

---

## 2. 前置依赖

### 2.1 运行时依赖

纯 CLI/Server 二进制不依赖 WebKit/GTK，但仍需：

| 依赖 | 说明 | 安装（Debian/Ubuntu） |
|------|------|----------------------|
| glibc ≥ 2.31 | C 运行时（Ubuntu 20.04+ 满足） | 系统自带 |
| OpenSSL | TLS（HTTPS 模型请求） | `sudo apt-get install -y libssl-dev` |
| ca-certificates | HTTPS 根证书 | `sudo apt-get install -y ca-certificates` |

### 2.2 编译依赖（仅源码安装需要）

| 依赖 | 说明 | 安装（Debian/Ubuntu） |
|------|------|----------------------|
| Rust toolchain（stable） | 编译器 | 见 [rustup.rs](https://rustup.rs/) |
| protoc ≥ 3 | Protocol Buffers 编译（Agent 协议） | `sudo apt-get install -y protobuf-compiler` |
| pkg-config | 库探测 | `sudo apt-get install -y pkg-config` |
| build-essential | C 编译器与 make | `sudo apt-get install -y build-essential` |

一键安装编译依赖：

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  protobuf-compiler \
  libssl-dev \
  ca-certificates \
  curl
```

---

## 3. 安装

### 3.1 源码编译（推荐）

```bash
# 1. 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. 克隆源码
git clone https://github.com/silent-rs/silent-Tiangong.git
cd silent-Tiangong

# 3. 编译 release 二进制
cargo build --release

# 4. 产物位于
ls -lh target/release/tiangong
```

产物 `target/release/tiangong` 是独立的纯二进制（不依赖 Tauri/WebKit），可直接部署。

### 3.2 部署二进制

将二进制放置到标准路径并赋予执行权限：

```bash
sudo install -m 0755 target/release/tiangong /usr/local/bin/tiangong

# 验证
tiangong --version
```

建议为天工创建专用系统用户运行（避免用 root）：

```bash
sudo useradd --system --create-home --shell /usr/sbin/nologin tiangong
```

---

## 4. 数据目录与权限

天工的所有配置、会话、记忆、日志都存储在用户家目录下的 `~/.tiangong/`：

```text
~/.tiangong/
  models.json           模型配置：Provider + Model + Routing
  server.json           Server 监听配置（host/port/auth_token）
  custom-prompt.md      自定义 Prompt（0.12.0 新增独立文件）
  skills.json           Skill 配置
  mcp.json              MCP 配置
  sessions/             会话持久化
  memory/               长期记忆数据（SQLite / Tantivy / 向量索引）
    config.json         Memory 独立配置
  logs/                 运行日志
  media/                生成或归档的媒体文件
  server.pid            后台守护进程 PID 文件
```

**关键特性**：配置与二进制完全解耦。更新二进制不会丢失任何配置、会话或记忆数据。

若使用专用 `tiangong` 用户，数据目录为 `/home/tiangong/.tiangong/`，确保该用户对其有读写权限。

---

## 5. 无界面配置流程

全部配置在网页配置页完成（详见 `docs/cli-configuration-guide.md`）。服务器上启动配置页，经 SSH 隧道在本地浏览器打开：

```bash
# 1. 本地电脑建立隧道
ssh -L 8800:127.0.0.1:8800 <服务器>

# 2. 服务器上启动配置页，在本地浏览器打开打印的链接
tiangong config --port 8800 --no-open
```

在页面中依次完成：

1. **模型服务**：添加 Provider（推荐用环境变量引用密钥，如 `DEEPSEEK_API_KEY`），点击「拉取模型」批量添加；
2. **模型**：确认能力并点击「测试」验证连通性；
3. **默认路由**：选择对话模型；
4. **Server**：设置监听地址、端口，生成 Token（只显示一次，请妥善保存）；
5. **插件管理 / 插件配置**：安装所需插件，并在插件配置中设置 MCP、Skills、记忆等；
6. 点击「完成并关闭」。

> 远程模式下，新登记的环境变量引用不能在页面中测试（防止服务器环境变量被发往任意地址）；请先在服务器环境中设置该变量后再启动配置页，或在页面中直接填写 API Key。

---

## 6. 运行与托管

### 6.1 前台运行（调试用）

```bash
tiangong server
```

指定监听参数（覆盖 server.json）：

```bash
tiangong server --host 0.0.0.0 --port 9000 --token tg_xxxxx
```

### 6.2 后台守护进程

```bash
tiangong server -d          # 后台启动，写入 server.pid
tiangong server stop        # 停止后台进程
```

守护进程日志输出到 `~/.tiangong/logs/server-daemon.log`。

### 6.3 systemd 托管（生产推荐）

创建 `/etc/systemd/system/tiangong.service`：

```ini
[Unit]
Description=Tiangong Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=tiangong
Group=tiangong
WorkingDirectory=/home/tiangong

# 方式一：通过 EnvironmentFile 注入密钥等环境变量
EnvironmentFile=/etc/tiangong/env

# 方式二：直接内联（不推荐密钥明文）
# Environment="DEEPSEEK_API_KEY=sk-xxx"

ExecStart=/usr/local/bin/tiangong server --host 127.0.0.1 --port 8080
Restart=on-failure
RestartSec=5

# 安全加固
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/home/tiangong/.tiangong
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

密钥通过 `/etc/tiangong/env`（权限 `0640 root:tiangong`）注入：

```bash
# /etc/tiangong/env
DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxx
ZHIPU_API_KEY=xxxxxxxxxxxxxxxx
```

启用并启动：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now tiangong
sudo systemctl status tiangong
sudo journalctl -u tiangong -f    # 查看日志
```

> 注意：使用 systemd 托管时，无需通过 `tiangong server -d` 启动守护进程（systemd 自身负责进程托管与重启）。直接用 `ExecStart=/usr/local/bin/tiangong server` 前台运行即可。

---

## 7. 反向代理

生产环境建议用 Nginx 反向代理，提供 TLS 终止与统一入口。

Nginx 配置示例：

```nginx
server {
    listen 443 ssl http2;
    server_name tiangong.example.com;

    ssl_certificate     /etc/letsencrypt/live/tiangong.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/tiangong.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;

        # WebSocket 支持
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";

        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # 流式响应
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }
}
```

调用方在请求头携带 Token（在配置页「Server」中生成，或查看 `~/.tiangong/server.json`）：

```bash
curl https://tiangong.example.com/api/v1/health \
  -H "Authorization: Bearer <token>"
```

---

## 8. API 速览

Server 模式提供 HTTP REST + WebSocket API，详见 `docs/server-api.md`。关键端点：

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/v1/health` | 健康检查（无需鉴权） |
| POST | `/api/v1/chat` | 对话 |
| WS | `/api/v1/ws` | WebSocket 流式对话 |

所有非 health 端点需在请求头携带 `Authorization: Bearer <token>`，Token 在 `tiangong config` 配置页的「Server」分区生成。

---

## 9. 更新策略

### 9.1 CLI 二进制的更新特点

天工的 `tiangong update` 命令（`crates/tiangong-entry/src/update.rs`）**仅检查版本**（`--check`），**CLI 二进制本身不能自动下载安装更新**（自动更新仅桌面应用支持，依赖 Tauri updater）。因此 Linux 服务器更新二进制需手动操作。

### 9.2 检查新版本

```bash
tiangong update --check
# 输出当前版本与最新版本，不执行更新
```

### 9.3 更新二进制

```bash
# 1. 拉取最新源码
cd /path/to/silent-Tiangong
git fetch --tags
git checkout v0.12.0   # 切到目标版本标签

# 2. 重新编译
cargo build --release

# 3. 停止服务
sudo systemctl stop tiangong   # 或 tiangong server stop

# 4. 替换二进制
sudo install -m 0755 target/release/tiangong /usr/local/bin/tiangong

# 5. 重启服务
sudo systemctl start tiangong

# 6. 验证
tiangong --version
curl -s http://127.0.0.1:8080/api/v1/health
```

**配置与数据不丢失**：所有配置、会话、记忆都存储在 `~/.tiangong/`，与二进制完全解耦。更新二进制只需替换可执行文件，数据目录保持不动。

### 9.4 滚动回滚

如新版本有问题，回退到上一个标签即可：

```bash
sudo systemctl stop tiangong
git checkout v0.11.0
cargo build --release
sudo install -m 0755 target/release/tiangong /usr/local/bin/tiangong
sudo systemctl start tiangong
```

---

## 10. 故障排查

| 现象 | 排查方式 |
|------|---------|
| Server 启动后无法访问 | `curl http://127.0.0.1:8080/api/v1/health`；`ss -tlnp \| grep 8080` |
| 模型请求失败 | 配置页「模型」中点击「测试」验证连通性；检查环境变量引用的密钥是否注入 |
| 鉴权失败 | 配置页「Server」确认是否设置 Token；请求头格式 `Authorization: Bearer <token>` |
| Memory 未生效 | 配置页「插件管理」确认记忆插件已启用，「插件配置 → 记忆」检查模型配置 |
| 插件异常 | 配置页「插件管理」查看插件状态与错误信息 |

日志位置：

- 前台运行：标准输出
- 守护进程：`~/.tiangong/logs/server-daemon.log`
- systemd：`journalctl -u tiangong`

---

## 11. 后续候选能力

以下能力在 RFC 0015 中列为后续候选（不在 0.12.0 范围）：

- **独立 CLI 二进制发布产物**：在 CI 新增 build job，产出 `tiangong-<version>-linux-<arch>.tar.gz`，免去服务器编译步骤。
- **CLI 二进制自更新**：扩展 `latest.json` 增加 CLI 通道，使 `tiangong update --apply` 可下载替换二进制。
- **官方 Docker 镜像**：基于 Alpine/Debian 预装天工二进制，开箱即用。
