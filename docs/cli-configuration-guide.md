# 天工 CLI 配置指南

> 关联文档：`docs/linux-server-deployment.md`（服务器部署）

天工的全部配置统一在**网页配置页**完成，命令行不再提供逐项配置命令。纯服务端环境（Linux 服务器、Docker、无桌面环境）同样通过网页配置：在服务器上启动配置页，在自己电脑的浏览器中打开即可。

配置写入 `~/.tiangong/`，CLI、Server 与桌面应用共享同一份配置。

---

## 命令总览

```bash
tiangong config     # 打开网页配置页（唯一配置入口）
tiangong cli        # 交互式对话；对话中输入 /config 同样打开配置页
tiangong server     # 启动 Server（-d 后台运行，server stop 停止）
tiangong bot start <id>   # 在后台启动已配置的 Bot（安装 / 配置 / 停止 / 升级 / 日志在配置页）
tiangong update     # 检查并安装天工更新
```

---

## 打开配置页 `tiangong config`

命令启动一个临时 HTTP 服务并打开浏览器，页面与桌面应用的设置页使用同一套组件（随桌面应用构建打包，`tiangong` 命令即桌面应用二进制）。配置完成后在页面点击"完成并关闭"，服务随之退出。

```bash
# 本机：自动打开浏览器
tiangong config

# 服务器：监听所有网卡、固定端口，只打印访问地址，在自己电脑的浏览器中打开
tiangong config --host 0.0.0.0 --port 8800 --no-open

# 或保持仅监听本机，通过 SSH 隧道访问
ssh -L 8800:127.0.0.1:8800 <服务器>     # 在本地电脑执行
tiangong config --port 8800 --no-open   # 在服务器执行，再在本地浏览器打开 http://127.0.0.1:8800/
```

| 参数 | 说明 |
|------|------|
| `--host` | 监听地址，缺省 `127.0.0.1`；远程配置可设为 `0.0.0.0` 或网卡地址 |
| `--port` | 监听端口，缺省随机 |
| `--no-open` | 不自动打开浏览器，只打印访问地址 |

配置页由用户自己临时打开、用完即关，不设访问令牌，页面与桌面设置页一样直接显示和编辑已保存的配置（包括 API Key）。在不可信网络中远程配置时请改用 SSH 隧道。

在 `tiangong cli` 对话中输入 `/config` 会在本机打开同一页面，关闭页面后自动重新加载配置。

---

## 配置页内容

| 分区 | 可完成的配置 |
|------|------|
| 智能体 | 新对话默认审核权限、默认工作区目录（填写天工所在机器上的路径） |
| 模型配置 | 供应商（Base URL、API Key、超时、请求头）、拉取模型列表、模型能力、模型路由；ChatGPT 账号登录（远程配置时使用设备码登录） |
| Server | 监听地址、端口、认证 Token（启停 Server 使用 `tiangong server` 或桌面应用） |
| Bot | 从线上目录安装 Bot、填写凭证或扫码授权（页面显示二维码）、启停、升级、日志、推送目标与 MCP 注册；在配置页启动的 Bot 以后台独立进程运行，关闭配置页不影响 |
| 沙箱 | 按需进程沙箱开关（关闭需确认）、沙箱程序状态与安装 / 更新、目录白名单（填写天工所在机器上的路径）、环境变量黑名单 |
| 插件管理 | 已安装插件的启用 / 停用 / 回滚 / 卸载；官方插件市场安装与升级；按路径导入本机插件目录或签名归档；可信发布者公钥管理 |
| 插件配置 | 各插件自带的配置页（与桌面端设置页同一页面），如 MCP 管理、Skills、记忆、定时任务、索引、火山引擎等 |

插件配置页与桌面端一致运行在隔离的 iframe 中，通过插件自身的 `plugin.*` 等接口与插件通信；配置页没有对话会话，不提供 sidecar、终端、浏览器等依赖会话的宿主能力。

---

## 配置文件结构

```text
~/.tiangong/
  models.json           模型配置：Provider + Model + Routing
  server.json           Server 监听配置（host/port/auth_token）
  app.json              默认信任模式、默认工作目录等
  custom-prompt.md      自定义 Prompt
  mcp.json              MCP 配置（MCP 插件管理）
  skills/               Skill 目录（Skill 插件管理）
  plugins/              已安装插件
  memory/
    config.json         Memory 配置（记忆插件管理）
  sessions/             会话持久化
  logs/                 运行日志
```

模型配置的 `api_key` 支持 `${ENV_VAR}` 环境变量引用，避免明文保存密钥。配置与二进制完全解耦，更新二进制不丢失任何配置。

---

## 典型流程：服务器首次配置

```bash
# 1. 本地电脑建立隧道
ssh -L 8800:127.0.0.1:8800 <服务器>

# 2. 服务器上打开配置页，在本地浏览器中完成：模型服务 → 模型 → 默认路由 → Server → 插件
tiangong config --port 8800 --no-open

# 3. 启动
tiangong server -d
```
