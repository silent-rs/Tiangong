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

天工 0.12.0 提供完整的模块化 CLI 配置能力（详见 `docs/rfc/0015-cli-modular-config.md`），无需桌面即可完成全部配置。完整流程：

```bash
# 1. 配置模型供应商（推荐用环境变量名引用密钥）
tiangong model add-provider deepseek \
  --protocol deepseek \
  --base-url https://api.deepseek.com \
  --api-key-env DEEPSEEK_API_KEY

# 2. 配置模型
tiangong model add-model deepseek-chat \
  --provider deepseek \
  --model-id deepseek-chat \
  --capability chat

# 3. 设置 chat 路由
tiangong model route set chat deepseek-chat

# 4. 验证模型连通性（真实请求一次）
tiangong model test chat

# 5. 配置 Server 监听
tiangong server config set --host 127.0.0.1 --port 8080

# 6. 生成鉴权 Token
tiangong server token generate
# 或指定长度：tiangong server token generate --length 48

# 7. （可选）配置自定义 Prompt
tiangong prompt set "总是使用简体中文回答，回复要简洁直接。"

# 8. （可选）配置 Memory：打印配置页链接，经 SSH 隧道在本地浏览器打开
#    本地执行：ssh -L 8800:127.0.0.1:8800 <服务器>
tiangong memory config --port 8800 --no-open
tiangong memory enable

# 9. 完整环境诊断
tiangong doctor
```

查看当前配置概览：

```bash
tiangong config path     # 列出所有配置文件路径
tiangong config show     # 配置概览
tiangong config validate # 本地结构校验
```

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
tiangong server status      # 查看状态（PID、进程存活、端口监听、Token）
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

调用方在请求头携带 Token：

```bash
curl https://tiangong.example.com/api/v1/health \
  -H "Authorization: Bearer $(tiangong server token show --raw)"
```

---

## 8. API 速览

Server 模式提供 HTTP REST + WebSocket API，详见 `docs/server-api.md`。关键端点：

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/v1/health` | 健康检查（无需鉴权） |
| POST | `/api/v1/chat` | 对话 |
| WS | `/api/v1/ws` | WebSocket 流式对话 |

所有非 health 端点需在请求头携带 `Authorization: Bearer <token>`，Token 通过 `tiangong server token generate` 生成。

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
tiangong doctor
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

| 现象 | 排查命令 |
|------|---------|
| Server 启动后无法访问 | `tiangong server status` 检查端口监听；`ss -tlnp \| grep 8080` |
| 模型请求失败 | `tiangong model test chat` 验证连通性；检查 `--api-key-env` 对应环境变量是否注入 |
| 鉴权失败 | `tiangong server token show` 确认 Token；请求头格式 `Authorization: Bearer <token>` |
| Memory 未生效 | `tiangong memory status`；确认无 `~/.tiangong/memory/.disabled` 标记文件 |
| 不确定哪里配置有问题 | `tiangong doctor`（加 `--deep` 做深度诊断） |

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
