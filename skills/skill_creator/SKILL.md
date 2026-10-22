# Skill 创建器

创建、导入和适配天工 Skill 的交互式工作流。支持从零创建新 Skill，也支持从 GitHub 等外部来源导入并适配为天工格式。

## 触发词

创建skill、新建技能、导入skill、创建一个skill、skill_creator

## Skill 存储目录

所有 Skill 安装到 `~/.tiangong/skills/<skill-id>/`（平铺布局，目录名必须等于 skill.toml 的 id）。

## 工作流

### 一、需求分析

1. 确认用户想创建什么功能的 Skill
2. 如果用户提供了外部 URL（如 GitHub 仓库），用 `run_command` 获取信息：
   ```bash
   curl -sL https://api.github.com/repos/{owner}/{repo}/contents/{path}
   ```
3. 分析外部 Skill 的功能、依赖和调用方式

### 二、创建 Skill 文件

在 `~/.tiangong/skills/<skill-id>/` 下用文件写入工具（`file_write`，action=write）创建以下文件。

#### 必需文件

**1. skill.toml** - 元数据声明
```toml
id = "my-skill"                    # 唯一 ID（kebab-case，必须与目录名一致）
name = "技能显示名称"               # 中文名称
version = "0.1.0"                   # 版本号
entry = "SKILL.md"                  # 入口文件
available = true                    # 是否启用

[source]
type = "local"
value = ""

[requires]
mcp = []
env = []

[permissions]
fs_read = []
fs_write = []
cmd_exec = []
net = []
```

**2. SKILL.md** - 技能使用说明（给 Agent 看的指令文档）
```markdown
# 技能名称

简要功能描述。

## 调用方法

使用 `run_command` 执行：
\`\`\`bash
python3 {skill_dir}/main.py --arg1 value1
\`\`\`

## 全部参数

| 参数 | 必需 | 说明 |
|------|------|------|
| `--arg1` | 是 | 参数说明 |

## 输出

JSON 格式：`{"ok": true, "result": "..."}`

## 触发词

关键词1、关键词2
```

> `{skill_dir}` 占位符在运行时会被替换为 skill 目录的绝对路径。

**3. 执行脚本** - Python/Shell 脚本（按需）
- Python 脚本使用 `argparse` 解析参数
- 输出 JSON 格式结果到 stdout
- 错误信息输出到 stderr

### 三、注册 MCP 服务器（如需）

如果 Skill 依赖 MCP 服务器，使用 `register_mcp_server` 工具：
- `name`: 服务器名称
- `command`: 启动命令完整路径（如 `/usr/local/bin/uvx`）
- `args`: 命令参数（如 `["mcp-server-name"]`）
- `env`: 环境变量（如 `{"API_KEY": "xxx"}`）
- `transport`: 传输方式（`stdio` 或 `http`）

### 四、验证与提示

创建完成后告知用户：
- Skill 名称和 ID
- 触发词
- 环境变量配置提示（如有）
- 提示用户：刷新 Skill 列表或开启新对话后生效

## 外部 Skill 适配指引

将外部 Skill（如 OpenAI Skills、Claude Skills）转换为天工格式：

1. **分析入口文件**：找到 README 或配置文件，理解功能和依赖
2. **提取执行逻辑**：识别核心脚本和参数
3. **创建 skill.toml**：填写 id、name、requires
4. **编写 SKILL.md**：将调用方式转换为 `run_command` + 脚本参数格式
5. **适配脚本**：确保脚本可通过命令行参数调用，输出 JSON
6. **处理依赖**：Python 脚本头部用 `/// script` 声明 PEP 723 依赖，或在 SKILL.md 中说明使用 `pipx run` / `uvx`
