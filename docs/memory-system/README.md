# 天工 Memory 系统设计

## 设计目标

天工 Memory 系统的核心目标不是"存更多"，而是：

- 低成本粗召回，按意图逐步深入
- 多层细节展开，上下文预算可控
- 可审计、可维护、可迁移
- 与现有 Prompt 装配和事件循环架构无缝融合

核心理念：

> Memory 不应一次性注入，而应像人类回忆一样逐步展开。

## 文档目录

| 文档 | 说明 |
|------|------|
| [01-记忆分层设计.md](01-记忆分层设计.md) | 记忆分层设计（7 层模型） |
| [02-注入层设计.md](02-注入层设计.md) | 注入层设计（Profile/Workspace/Session 三级注入） |
| [03-渐进式回忆.md](03-渐进式回忆.md) | 渐进式回忆流程 |
| [04-工作区文件索引.md](04-工作区文件索引.md) | 工作区文件索引 |
| [05-反刍与反思层.md](05-反刍与反思层.md) | 反刍与反思层 |
| [06-模块划分与代码归属.md](06-模块划分与代码归属.md) | 模块划分与代码归属 |
| [07-持久化格式与技术选型.md](07-持久化格式与技术选型.md) | 持久化格式与技术选型 |
| [08-并发安全策略.md](08-并发安全策略.md) | 并发安全策略 |
| [09-分阶段落地路径.md](09-分阶段落地路径.md) | 分阶段落地路径 |
| [10-预算控制与性能策略.md](10-预算控制与性能策略.md) | 预算控制与性能策略 |
| [11-当前开发状态与接手指南.md](11-当前开发状态与接手指南.md) | 当前开发状态与接手指南 |
| [12-缺口清单与专用MemoryLLM.md](12-缺口清单与专用MemoryLLM.md) | 缺口清单与专用 Memory LLM 约束 |
| [14-通用运行模式.md](14-通用运行模式.md) | MCP / 后台 Daemon / 配置页 / 自更新与第三方接入约定 |

## 与现有架构的关系

Memory 系统作为独立 crate `tiangong-memory` 实现，采用 **Actor 模型**独立运行，外部通过 `MemoryHandle` 消息通讯访问。

存储采用“**SQLite（加密）+ Tantivy（全文检索）+ 内置向量 / Qdrant（向量语义）**”三层架构：

| 层次 | 引擎 | 职责 |
|------|------|------|
| 元数据层 | SQLite（sqlcipher 加密） | 记忆管理、CRUD、生命周期 |
| 全文检索层 | Tantivy | BM25 关键词/短语召回 |
| 向量语义层 | Embedded flat vector（默认）或 Qdrant（可选） | 语义相似度召回、跨措辞匹配 |
| Injection 层 | Markdown 文件 | 人类可读的注入内容 |

> **降级策略**：当 Memory embedding 未配置时，向量语义检索和存储自动跳过；若配置了 rerank，则使用"SQLite + Tantivy + 模型精排"，否则降级为"SQLite + Tantivy"双层架构。

## 混合检索

默认测试链使用本地 deterministic embedding mock，不依赖外部服务：

```bash
cargo test -p tiangong-memory --test hybrid_retrieval_integration -- --nocapture
```

真实 embedding 路径保留为手动验证：

```bash
cargo test -p tiangong-memory --test hybrid_retrieval_integration embedded_hybrid_retrieval_loads_configured_embedding_and_recalls_semantic_episode -- --ignored --nocapture
```

可选 Qdrant 后端通过环境变量配置：

```bash
export QDRANT_URL=http://127.0.0.1:6334
export TIANGONG_MEMORY_QDRANT_COLLECTION=tiangong_memory
```

Memory 配置中将 `vector_mode` 设为 `external_qdrant` 后，写入、召回和归档删除会通过同一 Memory Actor 同步到 Qdrant。默认 `auto` 使用内置 embedded flat vector，无需启动外部服务。

## 专用 Memory LLM

Memory 内部的文本生成任务必须使用 `~/.tiangong/memory/config.json` 中的独立 Memory LLM，不得静默复用主 `chat` 模型或旧 `lite` 模型。涉及的任务包括 Episode 提取、Recall anchor 规划、Deep Recall 裁决、Recall 结果整理和 Meso Entity/Decision 提炼。

Embedding 与 Rerank 也必须通过 `tiangong-llm` 的 Provider 能力接入。`tiangong-memory` 只负责读取 Memory 独立配置、初始化召回引擎和处理降级，不直接实现向量或精排服务的协议适配。

当独立 Memory LLM 未配置时，Memory 只能降级到规则策略，并记录可诊断日志；主对话链路继续运行，但 Memory 的 LLM 增强能力视为关闭。

可手动运行 smoke test 验证真实 Memory LLM 配置是否可用：

```bash
cargo run -p tiangong-memory --example memory_llm_smoke
```

该命令默认读取 `~/.tiangong/memory/config.json`，校验模型返回的结构化标记，并打印 token 用量和耗时。没有配置真实模型时，可用 `--allow-missing-config` 只验证命令链路。

增量回忆已有固定样例集覆盖导出产物、迁移文件、图片、配置、性能排查和技能模板，确保 Tool 化回忆不会重复当前上下文已包含的内容。

Memory LLM 调用点会在日志中记录任务名、模型、协议、耗时和 token 用量，方便单独观察 Memory 成本和延迟。

Deep Recall 已有固定评测覆盖跨会话产物、Meso Entity、Meso Decision 和图关系追溯，确保需要深挖时能返回当前上下文之外的可执行线索。

## Workspace Index

Workspace Index 首期已落地在 `tiangong-memory` 中，支持：

- 生成并持久化最小文件树索引，按 `workspace_id` 隔离。
- 提取 Rust `mod/fn/struct/enum/trait` 符号。
- 查询文件和符号命中。
- 对单个文件执行增量更新。
- 在 `recall_memory` 输出中补充相关文件和符号线索。

```
crates/
  tiangong-memory/           ← 【独立 crate】Memory 基础设施
    src/
      lib.rs                 ← crate 入口，导出公共 API
      command.rs             ← MemoryCommand 消息协议
      handle.rs              ← MemoryHandle（客户端句柄，支持自动重连）
      actor.rs               ← MemoryActor（独立 tokio task 运行时）
      store.rs               ← MemoryStore（三层存储协调器）
      injection.rs           ← Injection 文件读写
      recall.rs              ← Progressive Recall（双引擎召回 + 融合重排）
      writer.rs              ← Episode/Decision 写入
      rumination.rs          ← 反刍（后期）
      workspace_index.rs     ← 工作区文件树与 Rust 符号索引
      db/                    ← SQLite 加密元数据库
      search/                ← 双引擎检索（Tantivy + Qdrant + Reranker）
      ipc/                   ← 跨进程 IPC 服务端/客户端
      election/              ← Leader 选举与迁移
```

**依赖方向**：
```
tiangong-cli、tiangong-server、src-tauri
  └─→ tiangong-memory   ← 启动时初始化 MemoryHandle
  └─→ tiangong-core     ← 将 Handle 传入 core
        └─→ tiangong-memory  ← 类型引用和 Handle 调用
        └─→ tiangong-llm     ← EmbeddingProvider / RerankProvider

tiangong-memory
  └─→ tiangong-llm     ← Memory LLM / Embedding / Rerank trait 引用
```

**通讯模型**：
```
调用方 ──(mpsc channel / IPC)──→ MemoryActor ──→ 磁盘读写
       ←(oneshot channel / IPC)── 查询响应
```

**所有运行模式都必须接入 Memory**，包括 GUI、TUI、Server、CLI，不允许任何模式跳过。当前 GUI、CLI、Server 创建 Core 时已统一通过 Memory election / IPC 获取 handle，同一 workspace 共享 leader，不同 workspace 使用独立运行文件；真实多进程集成测试已覆盖 CLI、GUI、Server 三入口共享 leader 与 follower 写入召回。

## 磁盘目录结构

```
~/.tiangong/
  memory/
    metadata.db                   # SQLite 加密数据库（元数据 + 详细数据）
    tantivy_index/                # Tantivy 全文索引目录
    qdrant/                       # Qdrant 向量索引目录（嵌入式模式）
    profile/
      agent.md
      preferences.json
    workspaces/
      <workspace_id>/
        workspace.json
        agent.md
        evidence/
    sessions/
      <session_id>/
        agent.md
    leader.lock
    leader.json
    registry.json
    memory.sock
  workspace-index/
    <workspace_id>/
      file-tree.json
      symbols.json
```

> Episode/Entity/Decision 的结构化数据存储在 SQLite 中，Evidence 因体积大仍使用文件存储。

## 分阶段交付

| 阶段 | 内容 | 存储引擎 | 代码变更量 | 详见 |
|------|------|---------|-----------|------|
| Phase A | Injection + 独立 crate + SQLite | SQLite | ~450 行 | [09](09-分阶段落地路径.md) |
| Phase B | Episode 写入 + IPC + Tantivy | +Tantivy | ~700 行 | [09](09-分阶段落地路径.md) |
| Phase C | Qdrant + 双引擎 Recall + Meso | +Qdrant | ~800 行 | [09](09-分阶段落地路径.md) |
| Phase D | Rumination + RerankProvider 精排 + Index | +ONNX | ~900 行 | [09](09-分阶段落地路径.md) |
