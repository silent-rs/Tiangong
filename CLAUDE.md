# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

**Silent-Tiangong（天工）** 是全功能可扩展的 GUI + CLI + Server 个人智能终端平台，实现"可对话、可规划、可执行、可扩展、可治理、可远程"的 Agent 能力闭环。支持通过 Connector 接入各类 IM 通道远程调度，并具备图片/视频等多媒体生成能力。

- **语言**: Rust (edition 2024)
- **架构**: Cargo workspace 多 crate
- **前端**: Tauri + React (GUI) / ratatui (TUI)
- **Server**: silent (HTTP/WS)
- **许可**: Apache License 2.0
- **存储位置**: `~/.tiangong/`

## 常用开发命令

```bash
# 构建
cargo build --release

# 检查（快速验证）
cargo check --workspace

# Lint（严格遵守 warnings）
cargo clippy --workspace --all-targets --tests --benches -- -D warnings

# 格式化
cargo fmt
cargo fmt -- --check  # 仅检查

# 依赖检查
cargo deny check --disable-fetch

# 测试
cargo nextest run --workspace
cargo nextest run --workspace --no-tests pass  # 无测试时通过

# 完整检查链（提交前自动执行）
cargo fmt -- --check && cargo check --workspace && cargo clippy --workspace --all-targets --tests --benches -- -D warnings && cargo nextest run --workspace --no-tests pass

# 运行应用
cargo run --release              # 桌面 UI 模式（默认）
cargo run --release -- cli       # CLI 模式
```

## 当前 Core 架构

现行说明见 `docs/core-architecture.md` 与 `docs/agent-loop-refactor/design.md`。

当前 `crates/tiangong-core` 的执行模型是：

- `TiangongCore` 接收用户消息和控制输入；
- 空闲用户消息通过 `start_user_turn` 从最新 Session 构建 `TurnContext`，随后创建一个 turn task；
- 运行中的用户消息通过当前 task 自有命令通道发送 `Command::InjectUserMessage`，在该 turn 内中断活动、保存新消息并重新分析；
- `shared_runtime` 只管理共享 Tokio runtime 和活跃 turn task 注册表；
- 每个 turn task 持有自己的上下文和命令接收端，结束后通道随之失效；
- 当前代码不存在常驻 Agent Driver 或 Agent Inbox。

核心模块：

- `crates/tiangong-core/src/core/`：Core 对外入口、插件装配和输入协调；
- `crates/tiangong-core/src/shared_runtime.rs`：共享 runtime 与 turn task 生命周期；
- `crates/tiangong-core/src/react/`：单轮 Agent Loop、命令、工具、压缩和收尾；
- `crates/tiangong-core/src/turn_context.rs`：单轮执行上下文；
- `crates/tiangong-core/src/session.rs`：会话消息和持久化状态；
- `crates/tiangong-core/src/config/`：core 配置（agent 运行时 / 最小配置契约 / 模型路由转发）；
- `crates/tiangong-core/src/tools/`：工具公共契约（结果类型与插件扩展点）。

审查或设计 Core 功能时必须以当前代码和上述两份现行文档为准，不得使用已经删除的 Driver/Inbox 迁移方案。

## 代码风格约定

### 导入顺序
```rust
// 1. 标准库
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// 2. 外部库
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

// 3. 内部模块
use crate::core::model::ModelClient;
use crate::core::session::Session;
```

### 命名约定
| 类型 | 风格 | 示例 |
|------|------|------|
| 结构体/枚举 | PascalCase | `TiangongState`, `RunStatus` |
| 函数/方法 | snake_case | `load_or_default()`, `execute_turn_with_streaming` |
| 常量 | SCREAMING_SNAKE_CASE | `DEFAULT_SESSION_TITLE` |
| 模块 | snake_case | `app_state`, `execution` |

### ID 与时间
```rust
// ID 生成必须使用 scru128，不用 UUID
let id = scru128::new().to_string();

// 时间使用本地时间
let now = chrono::Local::now().naive_local();
```

### 错误处理
```rust
use anyhow::{Context, Result, anyhow};

fn load_session(&self, id: &str) -> Result<Session> {
    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取会话文件失败：{}", path.display()))?;
    // ...
    Ok(session)
}
```

## 当前开发阶段

当前分支聚焦统一插件形态（Plugin Harness）。Agent Core 的现行架构以以下文档为准：

- `PLAN.md` - 项目总体规划和当前里程碑
- `TODO.md` - 当前任务列表
- `docs/requirements.md` - 需求边界与非目标
- `docs/core-architecture.md` - 当前 Core 执行模型与消息路由
- `docs/agent-loop-refactor/design.md` - 当前 Agent Core 专题说明
- `docs/plugin-harness/requirements.md` - Plugin Harness 需求
- `docs/plugin-harness-design.md` - Plugin Harness 设计

`docs/rfc/` 与 `docs/archive/` 中的历史方案仅用于追溯，不自动代表当前实现。

## 关键依赖

| 依赖 | 用途 |
|------|------|
| `anyhow` | 错误处理 |
| `serde` / `serde_json` | 序列化 |
| `chrono` | 时间处理（本地时间） |
| `scru128` | ID 生成（必须使用） |
| `async-openai` | OpenAI API 客户端（fork 版本） |
| `tokio` | 异步运行时 |
| `rmcp` | MCP 客户端 |
| `ratatui` | TUI 框架 |
| `silent` | HTTP/WS Server 框架 |
| `teloxide` | Telegram Bot SDK |
| `serenity` | Discord Bot SDK |

## 环境变量

| 变量 | 说明 |
|------|------|
| `API_AUTH_TOKEN` | API 认证令牌 |
| `API_BASE_URL` | API 基础 URL |
| `API_TIMEOUT_MS` | 请求超时（毫秒） |
| `API_MODEL` | 模型名称 |
| `API_STREAM` | 是否启用流式输出（默认 true） |
| `API_CLI_COMMAND` | CLI 模式命令 |

## 存储结构

```
~/.tiangong/
  skills.json           # Skill 配置
  skills-lock.json      # Skill 依赖锁文件
  mcp-lock.json         # MCP 依赖锁文件
  data/mcp/             # MCP 插件数据（mcp.json 配置、mcp-tools-cache.json 能力缓存）
  sessions/             # 会话持久化目录
```

## 常见问题

### RefCell 借用冲突
组件使用 `Rc<RefCell<>>` 包裹状态时，在闭包中访问要先 `borrow()` 提取需要的值，再 `borrow_mut()` 修改：
```rust
let (disabled, readonly) = {
    let s = self.state.borrow();
    (s.disabled, s.readonly)  // 先提取需要的值
};
// 再进行可变借用
let mut s = self.state.borrow_mut();
```

### 执行架构边界
`agents` 仅负责 planning/execution/response 等智能体推理，计划推进、结果归一化、验证执行等执行器逻辑需沉淀到 `execution` 领域模块，为后续基于配置文件的智能体装配与替换提供稳定边界。

### 动态 Step 推进
执行阶段采用动态 step 推进：初始计划仅保留可执行事项（plan item），每个 step 在执行后都需判断是否继续、补充或终止后续 step，避免预置 step 缺失导致误完成。
