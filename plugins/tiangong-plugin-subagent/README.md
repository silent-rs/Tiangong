# Subagent 插件

持久化 Subagent 统一管理（issue #480 阶段一）：长期 Agent 身份、跨会话激活、任务/运行管理与 Hook 反馈。

## 组成

- `plugin.json` — 14 个 AI 工具 + extension.tab 管理页 + resident Rust sidecar；
- `protocol/` — 宿主、UI 与 sidecar 共用的业务协议类型；
- `sidecar/` — 统一交互总线：`~/.tiangong/data/subagent/agents/` 身份目录、会话激活与 Workspace 写互斥、Task/Run 状态机、CLI Adapter（JSONL 子进程）、Hook 先落盘再投递；
- `src/` — Vue 3 管理页（筛选、三层状态、激活/停用/消息/任务/中断/取消、能力声明展示）。

## 数据目录

- 身份：`~/.tiangong/data/subagent/agents/<agent-id>/{agent.toml, instructions.md, memory/, artifacts/}`
- 运行态：`~/.tiangong/data/subagent/runtime/`（激活关系、任务、运行、事件历史、待投递 Hook 队列、worktree）

## CLI 后端协议

子进程 stdin 逐行接收 `begin` / `user_message` / `interrupt` / `cancel` JSON 帧；stdout 逐行输出 `message` / `status` / `blocked` / `approval_required` / `completed` / `failed` JSON 事件（非 JSON 行按 message 处理）；stdin EOF 时应自行退出。

## 构建与验证

```bash
cargo run -p xtask -- validate-plugin subagent
cargo run -p xtask -- build-plugin subagent   # 需官方签名密钥，产物部署到 ~/.tiangong/plugins/
```
