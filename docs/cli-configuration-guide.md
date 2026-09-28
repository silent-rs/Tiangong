# 天工 CLI 配置指南

> 适用版本：0.12.0+
> 关联文档：`docs/rfc/0015-cli-modular-config.md`（设计）、`docs/linux-server-deployment.md`（服务器部署）

天工提供完整的命令行配置能力，让纯服务端环境（Linux 服务器、Docker、无桌面环境）也能完成与桌面设置页等价的配置。

支持两种配置方式：

- **参数式命令**：适合脚本、CI、Docker、systemd 等自动化场景。
- **交互式向导**：适合 SSH 登录后手动配置，引导完成每一步。

两者写入同一份配置（`~/.tiangong/`），CLI 与桌面应用共享。

---

## 命令总览

```bash
tiangong model ...      # 模型配置（Provider / Model / Routing）
tiangong server ...     # Server 监听与 Token 配置
tiangong memory ...     # Memory 系统配置
tiangong prompt ...     # 自定义 Prompt 管理
tiangong config ...     # 通用配置查看与校验
tiangong doctor         # 环境诊断
```

---

## 1. 模型配置 `tiangong model`

模型配置采用 Provider / Model / Routing 三层结构：

- **Provider**：连接信息（协议、base_url、API Key）。
- **Model**：模型注册项（别名、model_id、能力）。
- **Routing**：路由槽位（chat / lite / embedding 等）指向某个模型。

### 参数式配置

```bash
# 1. 新增供应商（推荐用环境变量名引用密钥）
tiangong model add-provider deepseek \
  --protocol deepseek \
  --base-url https://api.deepseek.com \
  --api-key-env DEEPSEEK_API_KEY

# 2. 新增模型（声明能力）
tiangong model add-model deepseek-chat \
  --provider deepseek \
  --model-id deepseek-chat \
  --capability chat

# 3. 设置 chat 路由
tiangong model route set chat deepseek-chat
```

**API Key 两种方式**（互斥）：

```bash
# 方式一：环境变量名（推荐，写入为 ${VAR} 模板，运行时解析）
--api-key-env DEEPSEEK_API_KEY

# 方式二：明文（不推荐）
--api-key sk-xxxxxxxx
```

**其他操作**：

```bash
tiangong model list                  # 查看全部（providers / models / routes）
tiangong model list providers        # 只看供应商
tiangong model list models           # 只看模型
tiangong model list routes           # 只看路由

tiangong model route set lite deepseek-lite        # 设置轻量路由
tiangong model route set image_generation doubao   # 设置图片生成路由
tiangong model route list                          # 查看路由表

tiangong model remove-model deepseek-chat          # 删除模型
tiangong model remove-provider deepseek --force    # 强制删除供应商（连带引用项）

tiangong model validate               # 校验配置结构（路由引用、provider 存在性）
tiangong model test chat              # 测试 chat 路由连通性（真实请求 /models）
tiangong model test deepseek-chat     # 测试指定模型连通性
```

### 交互式向导

```bash
tiangong model configure
```

引导完成 provider → model → route 三步：选择协议（DeepSeek / OpenAI 兼容 / Anthropic）、输入供应商名称与 base_url、选择 API Key 方式、输入模型别名与能力（默认勾选 chat）、设置路由槽位。适合首次配置或不熟悉参数结构的用户。

---

## 2. Server 配置 `tiangong server`

### 参数式配置

```bash
# 查看 Server 配置（Token 脱敏）
tiangong server config show

# 修改监听地址与端口（可只改一项）
tiangong server config set --host 0.0.0.0 --port 9000
tiangong server config set --port 8080

# Token 管理
tiangong server token show                 # 查看脱敏 Token
tiangong server token generate             # 生成随机 Token（默认长度）
tiangong server token generate --length 48 # 指定长度
tiangong server token set tg_xxxxxxxxx     # 直接设置 Token

# 状态检查
tiangong server status                     # PID / 进程存活 / 端口监听 / Token

# 启动
tiangong server                            # 前台启动（使用 server.json 保存值）
tiangong server -d                         # 后台守护进程启动
tiangong server --host 0.0.0.0 --port 9000 # 命令行参数覆盖 server.json
tiangong server stop                       # 停止后台进程
```

**启动参数优先级**：命令行参数 > `server.json` 保存值 > 默认值（127.0.0.1:8080）。

### 交互式向导

```bash
tiangong server configure
```

引导设置监听地址、端口与 Token（生成随机 / 手动输入 / 跳过）。

---

## 3. Memory 配置 `tiangong memory`

Memory 的模型与检索配置统一在网页配置页完成（与天工桌面端设置页是同一份页面），命令行不再提供逐项参数。

```bash
# 打开配置页（本机浏览器），配置完成后点击"完成并关闭"
tiangong memory config

# 服务器 / 无图形环境：监听指定地址并只打印访问链接，在其他机器的浏览器中打开
tiangong memory config --host 0.0.0.0 --port 8800 --no-open

# 启用 / 禁用（标记文件，不丢失配置）
tiangong memory enable
tiangong memory disable

# 状态与测试
tiangong memory status    # 启用状态 + 当前模型
tiangong memory test      # 配置完整性与 ENV 可解析性检查
```

配置页可设置：
- **记忆文本模型**：跟随默认（lite → chat）、从 `models.json` 选择，或"自定义在线端点"（地址 / 模型名 / 协议 / API Key）。
- **嵌入模型 / 重排模型**：不启用、内置（按档位本地运行）或在线端点，支持探测维度与连通性。
- **向量模式**与**数据预览**（查看、编辑、归档记忆，召回测试）。

**远程配置安全提示**：配置页使用明文 HTTP，链接中带一次性令牌；`--host` 指向非回环地址时，页面中填写的 API Key 会在网络上传输。请只在可信内网使用，或保持缺省的 `127.0.0.1` 并通过 SSH 隧道访问：

```bash
# 在本地电脑执行，再在本地浏览器打开服务器打印的链接
ssh -L 8800:127.0.0.1:8800 <服务器>
# 服务器上执行
tiangong memory config --port 8800 --no-open
```

**禁用语义**：`disable` 创建标记文件 `~/.tiangong/memory/.disabled`，运行时（CLI / Server / Desktop）会跳过 Memory 启动；`enable` 删除标记。配置不丢失。

---

## 4. 自定义 Prompt `tiangong prompt`

自定义 Prompt 独立存储为 `~/.tiangong/custom-prompt.md`，便于 CLI 编辑与备份。

```bash
tiangong prompt show                    # 查看内容与字数
tiangong prompt set "..."               # 直接设置
tiangong prompt set --file ./prompt.md  # 从文件读取
tiangong prompt edit                    # 通过 $EDITOR 编辑（回退 vim/nano）
tiangong prompt clear                   # 清空
tiangong prompt path                    # 显示存储路径
```

自定义 Prompt 只从 `custom-prompt.md` 加载；文件不存在或内容为空时不设置自定义 Prompt。

---

## 5. 通用配置 `tiangong config`

```bash
tiangong config path       # 列出全部配置文件路径
tiangong config show       # 配置概览（模型 / Server / Memory / MCP / Skill / Prompt）
tiangong config validate   # 校验本地配置结构（不做外部连通性测试）
```

---

## 6. 环境诊断 `tiangong doctor`

```bash
tiangong doctor        # 默认不做真实网络请求
tiangong doctor --deep # 深度诊断（含模型连通性与端口探活）
```

聚合诊断配置目录、模型配置、密钥环境变量、Server、Token、Memory、MCP、Skill、自定义 Prompt。缺配置时给出具体修复命令。

**密钥环境变量检查**：`doctor` 会检查 `${VAR}` 形式的 API Key 是否能解析到真实值，发现未设置的环境变量会标记 ❌。这是纯服务端最常见的配置问题。

---

## 配置文件结构

```text
~/.tiangong/
  models.json           模型配置：Provider + Model + Routing
  server.json           Server 监听配置（host/port/auth_token）
  custom-prompt.md      自定义 Prompt（独立文件）
  skills.json           Skill 配置
  mcp.json              MCP 配置
  sessions/             会话持久化
  memory/
    config.json         Memory 独立配置（LLM / Embedding / Rerank 端点）
    .disabled           Memory 禁用标记（存在即禁用）
  logs/                 运行日志
  media/                生成或归档的媒体文件
  server.pid            后台守护进程 PID 文件
```

模型配置的 `api_key` 支持 `${ENV_VAR}` 环境变量引用，避免明文保存密钥。配置与二进制完全解耦，更新二进制不丢失任何配置。

---

## 典型配置流程

### 纯服务端首次配置（脚本化）

```bash
# 1. 配置模型
tiangong model add-provider deepseek \
  --protocol deepseek \
  --base-url https://api.deepseek.com \
  --api-key-env DEEPSEEK_API_KEY
tiangong model add-model deepseek-chat \
  --provider deepseek --model-id deepseek-chat --capability chat
tiangong model route set chat deepseek-chat

# 2. 配置 Server
tiangong server config set --host 127.0.0.1 --port 8080
tiangong server token generate

# 3. 配置 Memory（可选，服务器上用 --no-open 后在浏览器打开打印的链接）
tiangong memory config --no-open

# 4. 配置自定义 Prompt（可选）
tiangong prompt set "总是使用简体中文回答，回复要简洁直接。"

# 5. 诊断
tiangong doctor

# 6. 启动
tiangong server -d
```

### SSH 手动配置（交互式）

```bash
tiangong model configure    # 三步引导配模型
tiangong server configure   # 引导配 Server
tiangong memory config --no-open   # 网页配置 Memory
tiangong prompt edit        # 编辑器编辑 Prompt
tiangong doctor             # 检查环境
```

交互式向导在非 TTY 环境（管道 / 重定向 / CI / Docker）会自动检测并退出，不会卡住自动化流程。
