# 12 - 缺口清单与专用 Memory LLM

> 更新日期：2026-05-07

本文档记录当前 Memory 系统相对设计目标的剩余缺口，并冻结模型配置约束：Memory 内部所有模型调用必须使用独立 Memory 配置，不再复用主 `chat` 模型，也不挂在主 `models.json` routing 上。

## 当前完成度判断

当前 Memory 已经具备可运行主链路：

- 独立 `tiangong-memory` crate、Actor/Handle、SQLite、Tantivy、Injection 已落地。
- Core 已按 `workspace_id` 缓存 `MemoryHandle`，长生命周期 GUI/Server 进程不会把不同 workspace 误绑定到首个 Memory Actor。
- Micro 写入链路已经覆盖 `TurnResult -> EpisodeWriter -> SQLite/Tantivy/向量索引`。
- `recall_memory` 已作为 Tool 化按需回忆入口，结果作为消息上下文注入，不追加到稳定 system prompt。
- 结构化产物记忆已经覆盖媒体 URL、文件路径和工具结果摘要。
- Meso 第一阶段已能从近期 Episode 提炼 Entity/Decision，写入 SQLite/Tantivy，并更新 Workspace Injection。
- TCP loopback IPC、远端 `MemoryHandle`、leader election 和 follower failover 已有测试覆盖。

阶段完成度估算：

| 阶段 | 当前判断 | 说明 |
|------|----------|------|
| Phase A | 已完成 | 独立 crate、Injection、SQLite、Actor/Handle 主体均可用 |
| Phase B | 基本完成 | Episode 写入、Tantivy、IPC/election 已闭环，仍需主入口切换到选举模式 |
| Phase C | 约 65%-75% | 内置向量索引、可选 Qdrant、Deep Recall、Meso 均已有实现，但真实模型路径和质量评测不足 |
| Phase D | 部分推进 | Meta 生命周期、Workspace Index 首期和 RerankProvider 模型精排已落地 |

## 专用 Memory LLM 约束

Memory 不能再把主对话模型当成内部推理模型使用。主 `chat` 模型的职责是完成用户对话、工具决策和最终回复；Memory LLM 的职责是低成本、可控地处理记忆内部任务。

### 配置要求

Memory runtime 模型配置必须独立保存在 `~/.tiangong/memory/config.json`，由 `tiangong-memory` 提供配置类型、加载、保存和 `MemoryOptions` 转换能力。主 `models.json` 不再包含 `memory` capability 或 `routing.memory`；前端配置入口放在 LLM 配置下，选择项来自已有 Models，保存时由 Tauri 解析为 Memory runtime 端点配置。`embedding` 和 `rerank` 只作为 Models 能力标签供 Memory 选择，不进入 Routing 页。

示例：

```json
{
  "model": {
    "base_url": "https://api.example.com/v1",
    "api_key": "${MEMORY_API_KEY}",
    "model": "memory-small",
    "protocol": "openai_compatible",
    "timeout_ms": 60000
  },
  "embedding": {
    "base_url": "https://api.example.com/v1",
    "api_key": "${MEMORY_API_KEY}",
    "model": "bge-m3",
    "protocol": "openai_compatible",
    "timeout_ms": 60000,
    "dimension": 1024
  },
  "vector_mode": "embedded"
}
```

Memory 内部文本生成任务只允许读取独立 Memory LLM 配置：

- EpisodeWriter 结构化提取。
- Recall anchor 规划。
- Deep Recall 裁决。
- Recall 结果整理。
- Meso Entity/Decision 提炼。

### 降级规则

- 已配置 Memory LLM：使用专用 Memory LLM。
- 未配置 Memory LLM：Memory LLM 步骤降级为规则策略，并记录日志提示缺少 Memory LLM 配置。
- 禁止行为：未配置 Memory LLM 时静默复用 `chat` 主模型或旧 `lite` 模型。

### 设计动机

- 避免 Memory 的内部整理消耗主模型预算。
- 避免主模型能力、成本和延迟变化影响 Memory 稳定性。
- 允许用户为 Memory 单独选择小模型、本地模型或低成本服务。
- 让 Memory 的质量问题可以独立观测、独立调参、独立回退。

## 当前缺口清单

### P0：入口运行时缺口（已完成）

已完成：

1. Core workspace 级 Memory registry 已从直接调用 `start_with_options()` 切换为 `start_or_connect_with_options()`，保留专用 Memory LLM / Embedding / Vector 配置。
2. GUI、CLI、Server 创建 `TiangongCore` 时会显式传入入口类型，默认通过同一套 Memory election / IPC 路径获取 handle。
3. `tiangong-memory` 的 leader 运行文件已按 workspace 分区，同一 workspace 内仍保持单 leader，不同 workspace 不互相阻塞。
4. 已有测试覆盖同 workspace leader/follower、follower failover、Core registry workspace 隔离和配置热更新。
5. 已增加真实多进程验证：CLI、GUI、Server 三个子进程共享同一 workspace leader，follower 写入后可由父进程召回。

验收标准：

- 所有入口统一通过 `start_or_connect_with_options()` 获取 Memory。
- 同一 workspace 同时启动多个入口时只有一个 leader，其余入口使用 remote handle。
- leader 退出后 follower 自动接替，并且接替前后写入/召回都可用。
- 不同 workspace 的 Memory leader 运行文件相互隔离。

### P0：Memory 独立模型配置缺口（已完成）

已完成：

- `tiangong-memory` 已增加独立 `MemoryConfig`，持久化到 `~/.tiangong/memory/config.json`。
- Memory LLM、Embedding、Rerank 配置已从主 `models.json` / routing 拆出。
- `CoreConfig::to_memory_options()` 只读取 Memory 独立配置，不再从主模型配置派生。
- Tauri 已提供 `get_memory_config` / `set_memory_config`。
- 前端已将 Memory 配置放到 LLM 配置下，并从已有 Models 中选择 Memory LLM、Embedding、Rerank。
- EpisodeWriter、Recall anchor 规划、Deep Recall 裁决、Recall synthesis 和 Meso 提炼共享的 Memory model 来源已收口到专用 Memory LLM。
- Embedding 与 Rerank 均通过 `tiangong-llm` Provider 接入，Memory 侧只保存独立配置并调度召回链路，不再重复实现协议适配。
- 未配置 Memory LLM 时，MemoryOptions 不再携带文本模型，上述步骤全部走规则 fallback。
- 已补测试禁止未配置 Memory LLM 时静默复用 `chat` 主模型或旧 `lite` 模型。

### P1：真实模型路径验证（已完成固定链路）

1. EpisodeWriter、Deep Recall、Recall synthesis、Meso LLM 提炼已有代码路径。
2. 已增加 `plugins/tiangong-plugin-memory/core/examples/memory_llm_smoke.rs`，可手动读取 `~/.tiangong/memory/config.json` 调用专用 Memory LLM，并校验 JSON 标记、打印 token 用量和耗时。
3. 已补 6 个历史指代固定样例，覆盖导出产物、迁移文件、图片、配置、性能排查和技能模板，验证 Tool 化回忆只输出当前上下文之外的增量引用。
4. Memory LLM 调用点已统一记录任务名、模型、协议、耗时和 token 用量，覆盖 EpisodeWriter、Recall anchor、Deep Recall 裁决、Recall synthesis 和 Meso 提炼。
5. 已增加本地 OpenAI-compatible mock Memory LLM 固定评测，覆盖跨会话产物、Meso Entity、Meso Decision、图关系和 Deep Recall 结果整理。
6. 剩余风险主要是真实第三方模型 JSON 稳定性和失败回退仍需要长期样例观察。

验收标准：

- 增加可手动运行的 Memory LLM smoke test。
- 固定 5-10 个历史指代样例，验证输出只包含增量记忆。
- 记录 Memory LLM token 使用和延迟，确认不会拖慢主对话链路。
- Deep Recall 能通过固定场景追溯跨会话、跨产物、跨 Entity/Decision 的上下文。

### P1：混合检索质量缺口（已完成基础收口）

1. 内置 flat 向量索引已实现，并补充无外部服务的 deterministic embedding mock 集成测试。
2. 真实 embedding 测试继续保留为 ignored 手动验证，运行说明已补充到 README。
3. 已建立小型 recall benchmark，对比 BM25-only 与 hybrid 在固定语义样例上的命中率。
4. Qdrant 后端已具备 connect/upsert/search/delete，归档时通过统一 `VectorIndex::delete` 清理 point。
5. 服务配置通过 `QDRANT_URL` 和 `TIANGONG_MEMORY_QDRANT_COLLECTION` 控制，`vector_mode=external_qdrant` 时启用。
6. Recall 已支持通过 `tiangong-llm::RerankProvider` 对 BM25 或 hybrid 候选做模型精排；Embedding 不可用时可降级为 BM25 + Rerank。

验收标准：

- 默认测试链覆盖无外部服务的 embedded vector mock 或 deterministic provider。
- ignored 的真实 embedding 测试保留为手动验证，并补充运行说明。
- 建立小型 recall benchmark，比较 BM25-only 与 hybrid 的命中率。
- Qdrant 后端补齐归档删除同步和服务配置说明。

### P1：Meta 与生命周期缺口（已完成基础收口）

1. Meta 已覆盖低活跃节点归档。
2. 归档链路已能同步 SQLite 状态、Tantivy、embedded vector 和 Qdrant point 删除。
3. 过时检测已覆盖文件删除、路径失效、项目归档、产物 URL 过期等本地可判断场景。
4. Meta 执行结果通过结构化 tracing 字段输出 checked、archived、missing、expired 等计数。

验收标准：

- 归档时 SQLite、Tantivy、embedded vector、Qdrant 状态一致。
- 文件路径和媒体产物能做基本可达性检查。
- Meta 执行结果进入可观测日志和指标。

### P2：Workspace Index 缺口（首期已完成）

1. 已提供最小文件树索引，可生成、持久化、查询，并按 workspace_id 隔离。
2. Rust 符号索引已覆盖 `mod/fn/struct/enum/trait`。
3. `recall_memory` 无历史记忆或有历史记忆时，均可补充 workspace index 文件和符号线索。
4. 已提供单文件增量更新入口，文件变更后可刷新受影响的 file entry 与 Rust symbols。

验收标准：

- 最小文件树索引可生成、可查询、可按 workspace 隔离。
- Rust 符号索引至少覆盖 `mod/fn/struct/enum/trait`。
- Recall 可以在用户询问“之前改过哪个模块”时结合 Memory 与 workspace index 返回线索。

## 建议推进顺序

1. 先实现 Memory 独立模型配置，阻断继续复用主模型。
1. 运行 GUI + CLI + Server 的真实多进程组合验证，确认同 workspace leader/follower 行为。
2. 补 embedded vector 的可重复测试，再处理 Qdrant 删除和服务化配置。
3. 下一步补充 RerankProvider 真实模型质量评测和本地 ONNX embedding。

## 文档同步要求

后续实现上述任一缺口时，需要同步更新：

- `docs/requirements.md`
- `docs/memory-system/README.md`
- `docs/memory-system/09-分阶段落地路径.md`
- `docs/memory-system/11-当前开发状态与接手指南.md`
- 本文档的缺口状态
