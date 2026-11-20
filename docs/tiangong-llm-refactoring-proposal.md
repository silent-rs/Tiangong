# tiangong-llm 单次生成边界与 SDK 适配重构方案

> 状态：**修订评估稿；单一 stream、输入输出适配及简单任务无 Session 的方向已由讨论确认，尚未授权代码实施**。
>
> 本轮修订：对话支持直接传入 `&Session` 和本轮选项；标题等简单任务直接传入轻量请求；全部生成只走 `stream`，一次性结果由公共 collector 收集并适配为调用方类型。
>
> 现场基线：`598a7ab55be2aebc7fd4a200c8ccea55f38fb93c`，调查分支 `fix/stream-tool-call-parsing`。
>
> 本文是调整方案，不代表目标接口、目录、迁移和测试已经实现。本轮仅修订本文，不修改生产代码，不提交或推送，不替换现有架构约束。后续实施前应重新核验主线和工作区。

## 1. 评审摘要

### 1.1 推荐结论

**将 `tiangong-llm` 收敛为“单次流式生成”的稳定边界：调用方类型经输入适配进入唯一 `stream`，输出由实时消费者或公共 collector 转成调用方需要的结果；不负责会话管理、工具执行、模型路由和业务流程。**

不是重新增加一个统一 trait，也不是强行把所有 SDK 合并成一个通用实现，而是：

1. 演进现有 `LlmProvider`，只保留 `stream` 生成方法；删除独立的 complete/非流式执行链路。
2. 保留文本、工具调用、思考、签名、usage、停止原因和中断状态等必要语义。
3. 用响应内调用身份关联流事件，不再把上游 `call_id` 当作聚合主键。
4. 统一响应组装、错误语义和请求执行机制，保留薄协议适配器。
5. 将 SDK、认证、transport、配置装配和其他模型服务移出纯生成边界。
6. 分阶段迁移，先建立正确性回归，再迁移 API 和目录；不进行一次性全量重写。
7. Core 可直接传入借用的 Session；标题等任务使用轻量请求，不创建 Session。统一请求/结果是内部规范化边界，不要求业务流程显式拼装每层转换。

### 1.2 推荐的范围选择

| 选项 | 内容 | 判断 |
| --- | --- | --- |
| A：仅修解析问题 | 修复流身份和终态补全，保留现有架构 | 可单独交付，但不能完成职责收敛 |
| B：正确性修复 + 生成边界收敛 | 共享组装、统一 API、薄适配器、单一执行策略、分离非生成职责 | **推荐，本文完整范围** |
| C：重写所有 SDK、协议与调用方 | 替换第三方 SDK，所有功能一次迁移 | 不推荐，成本和兼容风险过高 |

可以只批准 B 中的前几个阶段，不必一次批准所有文件迁移。发布、提交、推送和真实付费 API 测试不包含在本文授权范围中。

### 1.3 必须保持的约束

- Core 继续负责会话和新增工具调用的跨轮唯一化；不恢复此前已撤回的历史 ID 改写方案。
- 不修改旧会话，不做落盘迁移，不在请求副本中重新分配历史调用 ID。
- 不按名称或参数相同去重工具调用；重复调用可能是合法输出。
- 不删除 Responses、Chat Completions、Anthropic、DeepSeek 或 Codex 的现有能力。
- 不让 Core 依赖第三方 SDK 类型。
- 不为了缩短代码而吞掉异常调用、思考签名、usage、停止原因或中断状态。
- 不借本次重构更换 SDK 版本、变更默认模型、改变协议序列化名称或扩大网络权限。
- 只有一个流式生成接口，不要求所有业务展示增量；完整 JSON 解析只作为流式入口的兼容能力，不恢复独立非流式执行路径。

## 2. 现状与证据

### 2.1 已存在的抽象

以下链接相对本文所在的 `docs/` 目录，可直接定位当前基线的文件：

| 位置 | 当前职责 | 调整判断 |
| --- | --- | --- |
| [provider.rs](../crates/tiangong-llm/src/provider.rs) | `LlmProvider`：能力、complete、stream、list_models | 已有统一接口，不应另建一层同义 trait |
| [request.rs](../crates/tiangong-llm/src/request.rs) | `ProviderRequest`、思考选项 | 统一生成请求的基础 |
| [response.rs](../crates/tiangong-llm/src/response.rs) | `ProviderResponse`、`StopReason` | 统一响应的基础 |
| [provider_client.rs](../crates/tiangong-llm/src/provider_client.rs) | `ModelClient`、`ModelRequest`、`ModelResponse`、构建、转换、流组装、重试、校验、统计 | 多职责集中，需要拆解并消除重复模型 |
| [stream.rs](../crates/tiangong-llm/src/stream.rs) | 公共事件、完整 JSON 合成流 | 已有统一事件，但调用身份不充分 |
| [message.rs](../crates/tiangong-llm/src/message.rs) | 文本、思考、签名、工具、图片和文件内容 | 优先复用，不重新发明一套消息模型 |
| [providers/mod.rs](../crates/tiangong-llm/src/providers/mod.rs) | 五种适配器 | 按协议/SDK 保留差异，抽掉重复机制 |
| [endpoint.rs](../crates/tiangong-llm/src/endpoint.rs)、[models_config.rs](../crates/tiangong-llm/src/models_config.rs) | 端点、模型注册表与解析 | 配置装配不属于单次生成领域模型 |
| [text.rs](../crates/tiangong-llm/src/text.rs) | 文本便捷入口、另一套端点配置 | 改为统一生成入口的薄包装 |
| [embedding.rs](../crates/tiangong-llm/src/embedding.rs)、[rerank.rs](../crates/tiangong-llm/src/rerank.rs) | 向量化、重排 | 独立服务，不加入生成 trait |

当前接口统一并不等于行为统一：

- Anthropic、DeepSeek client 分别实现相似的重试、退避、日志和通知循环。
- OpenAI Responses、Chat Completions client 也分别维护类似循环。
- `ProviderDispatch` 在已有 `LlmProvider` 的情况下又逐项转发 complete/stream。
- 公共流消费器和 Codex 完整响应收集器分别聚合工具调用。
- 流式和非流式经由不同收集路径，异常事件的处理不一致。
- `ModelResponse` 中既有模型结果，也有业务用量类型、工具校验结果、TTFT 和展示统计。

### 2.2 现有架构文档需要同步，但不能把草案当成既定规则

[现有架构约束](tiangong-llm-architecture.md)已经要求统一抽象、薄 provider、transport 下沉，与本方案方向一致。

其中“Responses 尚未合入、主线仅启用 Chat Completions”等现状描述与本次基线不符；当前五种适配器均存在。本文先记录差异，**不在评审阶段改写现有规范**。方案批准后，在对应实施阶段同步原文，避免两个文件长期表达不同规则。

### 2.3 已复现的正确性问题

基线上的代码路径：

1. Chat Completions decoder 按 index 保留不同调用，但发到公共聚合器后按 `call_id` 插入 Map；同 ID 的后一个 Start 覆盖前一个。
2. 流式入口收到完整 JSON 时，完整响应会转成 Start 事件；重复 ID 仍在公共聚合器覆盖。**非流式 JSON 正常，不代表 JSON 合成流正常。**
3. DeepSeek Responses 用 `call_id` 记录 started/received 状态，不同输出项可能共享状态；ID 后到或缺少 added 时，会丢参数或生成额外空调用。
4. OpenAI Responses 的 completed 兜底不补完整 Start；只有完整终态的合法调用可能被过滤为空。
5. Core 的唯一化发生在模型响应返回之后，无法恢复 LLM 已覆盖的调用。

上一轮本地 HTTP mock 的 26 个场景结果文件在本轮仍可读取：

- 复现源码：`/tmp/tiangong-stream-parse-probe.LthG8o/src/main.rs`
- 结果：`/tmp/tiangong-stream-parse-probe.LthG8o/results.jsonl`
- 共 26 项，10 项符合预期，16 项出现调用丢失或参数损坏。

初版文档编写时重新读取了结果文件并核对统计，本轮修订未重新运行探针。临时目录不是长期验收依据；实施时应将最小场景转成仓库内正式测试。上述为构造事件的确定性复现，**不是线上原始日志，不能据此断言某网关线上具体触发了哪种异常。**

## 3. 目标与非目标

### 3.1 目标

对调用方，生成只有一个概念：

```text
Session / 轻量请求 → 输入适配 → 一次 stream 调用 → 实时消费 / 收集并转换结果
```

“单次调用”是逻辑操作，不代表一定只有一次 HTTP 请求；明确配置的安全重试可以发生，但不得扩展为多轮 Agent 流程。

完成后的关键性质：

- 只有一个 stream 生成接口、一套内部请求、事件、收集结果、错误和终态语义。
- SSE、WS（适用时）与网关 JSON 合成流输出等价语义，不新增非流式执行链路。
- Session、标题等输入共享同一路径；调用方结果适配不要求生成层依赖 Core。
- 调用方无需按 SDK 分支处理模型内容。
- 上游调用 ID 冲突不会导致调用覆盖；历史唯一化边界不变。
- 适配器只映射协议和错误，不各自复制执行框架。
- 正确性有跨适配器契约测试，稳定性收益可复核。

### 3.2 非目标

- 自动选择模型、故障时换供应商、模型路由、总结触发策略。
- 工具执行、授权、审批、重放、并发调度和结果落盘。
- 重写会话历史、修复旧会话、数据库迁移。
- 新增模型能力、引入新供应商或扩展未使用的多模态协议。
- 以减少文件数或总行数作为唯一验收指标。
- 为每个协议增加一个新 crate，或建立通用插件化 provider 注册框架。

## 4. 职责边界与依赖方向

### 4.1 最终职责表

| 层 | 负责 | 不负责 |
| --- | --- | --- |
| Core / Context / Tool runtime | 实现 Session 输入与 Core 输出适配、上下文预算、系统提示策略、工具校验与执行、调用 ID 唯一化、多轮循环 | SDK 类型、HTTP/SSE 帧解析 |
| `tiangong-llm` | 单一 stream 契约、输入输出适配契约、统一事件、错误语义、响应内身份、共享组装器与 collector | 理解或依赖 Core Session、凭据、模型注册表、路由、工具执行、SDK client 构建 |
| 集成层 | 适配器工厂、端点配置、SDK 映射、共享请求执行策略、模型目录、认证接入、embedding/rerank 独立接口 | 会话流程、业务提示词、工具执行 |
| SDK / transport | 原始协议类型、HTTP/SSE/WS、厂商协议解码、私有文本协议兼容 | Core 会话类型、工具运行时、业务路由 |
| 调用观测包装层 | 延迟、重试通知、日志、流速统计和界面事件转换 | 改写模型输出、吞掉模型错误 |

### 4.2 推荐的物理结构

**推荐最终增加一个集成 crate，暂名 `tiangong-llm-adapters`；不是第一阶段就搬文件，也不再同时增加 config/auth/catalog 等多个 crate。** 新 crate 的价值是让生成契约不反向依赖所有 SDK，而不只是换目录。

```text
Core / CLI / Server / App / Memory 调用方
    ├── tiangong-llm：请求、响应、流、错误、组装器
    └── tiangong-llm-adapters：在装配位置创建具体 client
            ├── tiangong-llm
            ├── tiangong-anthropic
            ├── tiangong-deepseek
            └── async-openai / 必要 transport
```

依赖要求：

- `tiangong-llm` 不依赖 adapters、Core 或第三方 SDK。
- 原生 SDK 不反向依赖 `tiangong-llm`；统一类型到 SDK 类型的实现放在 adapters，避免循环依赖。
- Core 执行路径使用带输入/输出适配的 client facade，其内部持有 `Arc<dyn Llm>`；Session 和 Core 输出转换实现留在 Core，只有创建 SDK client 的装配位置依赖 adapters。
- 配置解析与模型注册表暂集中到 adapters 的配置模块；“选哪个模型”的业务决定仍由上层完成。
- embedding/rerank、模型目录暂放在同一集成 crate 的独立模块和 trait 中，不加入 `Llm`。
- 不把无明确归属的配置和认证塞进 `tiangong-types`。

此布局相对现有文档的变化是：`tiangong-llm` 从“契约 + provider facade”进一步收敛为“契约 + 无供应商依赖的响应组装”。需要评审同意后才调整既有架构规范。

SDK transport 暂无独立承载位置时，可以先留在 adapters 的私有 transport 模块，不为搬代码新写 SDK。OpenAI transport 是否另拆 crate，应在复用需求和 API 稳定后决定，不作为本次前置条件。

## 5. 统一生成接口

以下 Rust 均为**接口设计示意，不是可直接应用的最终代码**。具体字段名和类型在迁移前通过调用方编译样例确定；不能照抄示意再长期保留两套相同模型。

### 5.1 唯一的 stream 生成 trait

```rust
#[async_trait]
pub trait Llm: Send + Sync {
    fn capabilities(&self) -> GenerationCapabilities;

    async fn stream(&self, request: LlmRequest)
        -> Result<LlmStream, LlmError>;
}
```

- 从现有 `LlmProvider` 演进，不叠加同义 trait；**不保留独立 complete 方法或非流式生成实现**。
- SDK 在本项目只适配一个流式生成接口；SDK 自身的非流式 API 不要求删除，但不再构成本项目的另一条执行路径。
- 一次性结果通过流消费得到，不是第二种生成协议；同步 CLI 也只阻塞等待同一流及 collector。
- 固定的内部统一类型保留底层 trait 的 dyn 兼容性；泛型转换放在 client facade 或消费扩展层，不放进动态分发方法。
- trait 不要求 Session 对象、工具执行器、GUI callback 或模型注册表。
- `capabilities` 只说明适配器支持的生成行为；模型级实际能力由构建时配置或已知元信息约束，不能把“协议支持”当作“所有模型都支持”。
- `list_models` 改成独立 `ModelCatalog`；协议枚举留在工厂/配置处，不另用枚举重复转发生成方法。

#### 5.1.1 转换驱动的输入输出

此前 `Into<dyn Request>`、`From<dyn Response>` 是转换边界的伪代码意图，不是要求所有请求/响应都成为运行时 trait object。

```text
Core 的 &Session + 本轮选项 ── Core 输入适配 ──┐
简单文本请求 + 生成选项 ───── 轻量输入适配 ───┤
                                           ▼
                              client facade → LlmRequest
                                           │
                                    唯一 Llm::stream
                                           │
                                       LlmStream
                           ┌───────────────┴──────────────┐
                     实时事件映射                     统一 collector
                           │                              │
                   Core/界面可消费事件            内部 LlmResponse
                                                          │ 输出适配
                                              CoreModelOutput / 文本结果
```

- 无损且不会失败的转换用 From/Into；可能缺字段或不支持时用 TryFrom/TryInto。需要本轮选项和资源准备时用显式本地适配 trait/mapper，不强迫统一语法。
- Session 输入适配由 Core 实现；client 调用契约，但 LLM crate 不认识 Session，不承担会话存储。Core 不再在 ReAct 流程中显式拼装每层中间模型。
- Core 定义自己的结果和事件转换；收集/映射层返回 Core 可直接处理的类型，不要求 Core 分支识别 SDK。
- 第三方 SDK 与统一类型都不是 adapters 的本地类型时，标准转换可能受孤儿规则限制；使用本地 mapper/newtype，不反向引入 Core 依赖。
- 输入准备完成后不长期借用 Session，不跨网络等待持有会话锁，不为转换复制整个 Session；Send/lifetime 与附件准备在 P0 编译样例中验证。
- 内部规范化模型仍是权威边界；不是通过 Any/downcast 在 Core 中操作 SDK 请求。

#### 5.1.2 对话与简单任务的调用体验

以下是目标体验示意，不代表已实现 API。对话直接传借用的 Session 和本轮选项，在同一次流消费中展示增量并收集 Core 结果：

```rust
let output: CoreModelOutput = client
    .stream(&session, &turn_options)
    .await?
    .collect_with(on_event)
    .await?;
```

`turn_options` 提供可用工具、tool choice、思考强度、输出预算等不全部属于 Session 的信息。结果到达后由 Core 校验、分配最终调用 ID、写入会话和执行工具；转换不自动修改 Session。

标题等任务不创建临时 Session：

```rust
let title = client
    .stream(
        TextRequest {
            instruction: "生成一个简短的标题",
            input: source_text,
        },
        &title_options,
    )
    .await?
    .collect_text()
    .await?;
```

标题任务默认不声明工具，不携带完整历史、权限或 Agent 状态；可显式关闭思考和限制预算，不套用对话主任务的隐式默认。标题清洗、长度规则、空结果回退属于标题模块，不进入 collector。

**简单任务等待最终结果，不代表非流式调用；不展示增量也不需要把请求或结果放入 Session。**

#### 5.1.3 公共 collector 与输出契约

- 实时消费、完整结果收集、纯文本收集复用同一状态机；`collect_text` 不维护独立终态解析器。
- 一条流只消费一次；`collect_with` 在组装时同步发出映射后的增量，不再发请求，也不二次遍历网络流。
- collector 不调用 SDK complete，不新增自动业务重试。一次性结果只是流的消费方式。
- 完整 collector 保留 output、usage、stop_reason、status 和诊断，通过 Core 转换得到 CoreModelOutput；ReAct 直接消费该类型。
- `collect_text` 默认只接受正常完成且符合纯文本预期的结果；中断、截断或意外工具调用报错，不把部分文本当成成功。接受 partial 必须显式选择。
- String helper 可以存在；需用量的任务选择带元信息的结果，不因隐藏增量而丢 usage 或伪造零值。
- observer 失败与取消应传播并停止读取；不后台继续生成，不无界缓存。缓冲/背压按消费者能力在 P0 定稿。

### 5.2 内部规范化请求与请求执行选项分开

```rust
pub struct LlmRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: Option<ToolChoice>,
    pub generation: GenerationOptions,
    pub hints: RequestHints,
}
```

建议：

- 复用 `ChatMessage`、内容块、`ToolSpec`、`ToolChoice` 和 `ReasoningEffort`。
- `LlmRequest` 是输入适配后的内部结构，不要求 Core 业务逐字段构造；轻量请求复用公共适配，Session 适配由 Core 实现。
- `generation` 包括 max output tokens、temperature、top_p、stop sequences、reasoning effort 等已有语义。
- model 在请求中显式给出；旧 `SingleProviderClient` 的绑定模型由兼容包装填入，避免请求与 client 两处互相覆盖。
- `system` 暂保留独立入口；历史 System role 的规范化沿用已有映射，不能在迁移中悄悄改变提示词优先级。
- 原 `session_id` 的缓存路由用途映射为 `hints.cache_key`；它是调用方提供的不可解释标识，不代表 LLM 持有会话。追踪标识与缓存键分离。
- api key、base URL、代理、HTTP headers、重试和超时不进入模型消息，归 client 构建配置/执行选项。
- 请求级 timeout 需求通过单独的 execution options 或已配置 client 包装保留，不删除原能力。初期兼容旧调用签名，最终 API 具体传递方式在阶段 P0 固定。
- 不默认从环境变量读取温度或 token 上限。旧入口先保持原有默认解析，迁移后由装配层显式提供，并用快照证明等价。
- 附件内容由 Core/调用方准备；LLM 不读取本地文件、不提取 system prompt、不增加业务兜底 User 消息。

### 5.3 流收集结果应完整，不构成第二种生成接口

`LlmResponse` 是统一 collector 的规范化结果，不是 `Llm::complete` 的返回类型。Core 可经转换直接收到自己的结果；简单任务只取文本时也须完整校验终态。

```rust
pub struct LlmResponse {
    pub id: Option<String>,
    pub model: Option<String>,
    pub output: Vec<OutputBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Option<TokenUsageData>,
    pub status: CompletionStatus,
    pub diagnostics: Vec<ResponseDiagnostic>,
}
```

设计原则：

- output 是权威内容源；`text()`、`tool_calls()` 是派生访问，不再维护可互相矛盾的平行字段。
- 同一次响应可以同时包含正文和多个工具调用，不设计为二选一。
- 优先复用现有内容块结构；只有输出需要额外表达工具有效性时才增加最小输出专用封装。
- 保留思考、redacted thinking、签名及已有多模态能力；不把已有消息类型误当成所有协议都支持的输出能力。
- usage 缺失用 `None`，不冒充服务端返回零用量；业务用量类型转换移到调用方。
- stop_reason 表示模型停止原因；status 表示这次输出是否完整，二者不能互相替代。
- 响应原始 JSON 不作为稳定业务 API；需要诊断时采用显式启用、脱敏和限量保留，不默认复制整份响应。

`CompletionStatus` 至少区分：

| 状态 | 含义 | Core 能否执行工具 |
| --- | --- | --- |
| Complete | 已观察到协议认可的终态 | 仍需检查具体调用完整性和 schema |
| Incomplete | 模型受输出限制等原因未完整结束 | 仅可考虑明确独立结束且有效的调用，不执行未结束项 |
| Interrupted | 连接/服务错误中断，保留部分结果 | 不自动执行仍在构建的调用；具体恢复由 Core 决定 |

终态缺失的 EOF 不应一律视为成功；每个协议定义合法结束条件。例如 Chat Completions 的 finish_reason 可以构成语义终态，不应强制所有协议都发送 Responses 风格的 completed。

### 5.4 无效工具调用必须显式表达

- 参数 JSON 解码、缺失名称、缺失终态等是输出完整性问题，LLM 层返回结构化诊断/无效调用结果。
- 工具是否注册、是否允许执行、是否满足 schema、是否获得权限，由 Core/工具校验层处理，适用于所有协议。
- 不再把 `__parse_error` 注入工具参数伪装成正常 arguments；过渡期由兼容包装生成旧格式，直至 Core 恢复链路迁移完成。
- 无效调用与原始候选仍可复核，不静默删除；必要的原始参数只在受控响应诊断中保留，默认日志不打印。
- 空参数能否解释为 `{}`，必须结合“调用已完整结束且协议允许无参”判断；缺失参数不能单靠空串自动判为合法无参调用。

### 5.5 兼容映射清单

| 当前字段/行为 | 目标归属 | 迁移要求 |
| --- | --- | --- |
| `ModelRequest` → `ProviderRequest` | 一套 `LlmRequest` | 包装层处理旧默认值，不能长期保留双权威模型 |
| `ModelResponse.text/tool_calls/reasoning_*` | output 派生访问 | 保留顺序、签名及现有消费语义 |
| `ProviderResponse.raw` | 可选诊断设施 | 非稳定业务依赖，不默认记录敏感内容 |
| `TokenUsage` 与 `TokenUsageData` 转换 | 调用方兼容/观测层 | 不丢 cache hit/miss 等已有字段 |
| `InvalidToolCall` | LLM 结构完整性 + Core 业务校验 | 先保留旧链路，再逐项迁移 |
| TTFT、decode、reasoning elapsed、累计 token 近似值 | 观测包装与上层事件 | 明确计时/计数口径，不当作模型正文 |
| `ModelStreamChunk` | 统一事件到旧 callback 的桥接 | 不更改旧界面事件协议 |
| `api_base_url/api_timeout_ms/api_model` | client 配置描述或上层已知配置 | 不要求生成 trait 暴露凭据/连接细节 |
| `complete_text*` | `stream + collect_text` 的临时兼容包装 | 不调用非流式 SDK；迁移完成后删除旧命名入口 |
| `Session.context().into_model_request()` | Core 实现输入适配，client facade 自动调用 | 上下文/附件/文件读取逻辑不搬入 LLM |
| SDK 非流式 complete | 从本项目生成执行路径移除 | SDK API 本身不要求删除；JSON 解码仅为 stream 兼容保留 |
| 标题等轻量调用 | 简单输入 → stream → collector → 业务结果 | 不创建/写入 Session，不要求 UI 展示增量 |

## 6. 流式正确性设计

这是稳定性收益的核心，优先于目录重排。

### 6.1 三种身份必须分离

| 身份 | 作用域 | 负责方 |
| --- | --- | --- |
| `CallKey` | 一次响应内部，关联调用各阶段 | SDK 事件适配器分配，组装器使用 |
| 上游 `call_id` | 协议给出的工具结果配对 ID，可缺失或重复 | 适配器保留为元数据 |
| 会话最终 ToolCall.id | 进入 Core 后的新批次，需与历史唯一 | 现有 Core `assign_tool_call_ids` |

`CallKey` 不是上游 call_id 的重命名，也不是历史 ID 分配器。建议使用响应内单调递增的整数新类型；与原始 index/item_id 建立映射，不通过字符串后缀碰撞规避。

流式请求收到完整 JSON 时，每个列表项按位置获得独立 CallKey。历史非流式基线仅作迁移对照，不继续维护非流式执行入口。相同 call_id、相同名称和相同参数都不得合并两个独立输出项。

缺失上游 ID 时，可以保留现有响应内稳定占位值作为兼容结果；**完整响应和流组装均先确定独立条目，Core 再按现有规则处理新增 ID**。占位值不能重新成为聚合主键。

### 6.2 统一事件应区分增量与终态快照

事件概念示意：

```text
MessageStarted
ContentDelta / ReasoningDelta / SignatureDelta
ToolCallMetadata { key, upstream_id?, name? }
ToolArgumentsDelta { key, fragment }
ToolArgumentsSnapshot { key, complete_json }
ToolCallFinished { key }
UsageUpdated
Finished { stop_reason, status }
```

- 允许先收到参数、再补名称或 call_id。
- 完整参数快照不是增量，不能追加到已收到的 JSON 后面。
- Metadata 更新不重置参数；重复同值更新幂等。
- stream 的传输/解析失败使用结构化错误通道，不同时保留字符串 Error 事件和语义重复的 Err。
- 每个成功或模型主动结束的流最多一个 Finished；transport 失败可以在部分事件后返回 Err，由收集器生成 Interrupted 结果。取消导致 future/stream 被丢弃时，不强行保证还能推送终态。
- 统一收集器提供“保留 partial + error”的结果；调用方不必把中断包装成正常成功。

### 6.3 共享组装器规则

| 场景 | 明确行为 |
| --- | --- |
| 相同 call_id、不同输出项 | 按不同 CallKey 保留，不覆盖 |
| 同一输出项重复 added/Start | 更新缺失元数据，不重新创建或清空参数 |
| Delta 早于 Start | 保存 pending，等待元数据/终态补齐 |
| call_id 在 done/completed 才出现 | 更新该 CallKey 的上游 ID，不迁移或复制调用 |
| 只有 completed | 从终态中完整项目补元数据和参数快照 |
| 已有增量，又有完整终态参数 | 使用完整快照定稿一次；不拼接，冲突有诊断 |
| 终态快照与增量不一致 | 优先同一输出项明确有效的终态快照并给出诊断；快照无效时不静默退回猜测结果，标记该调用无效 |
| 缺少名称或参数未闭合 | 保留诊断和无效候选，不当作正常可执行调用 |
| 流意外 EOF | 保留 partial，按协议终态判断 Complete/Interrupted |
| 相同调用的完整终态重复 | 幂等，不重复生成调用 |
| 多个调用增量交错 | 独立缓存、独立定稿，顺序按协议输出位置 |

不得采用“当前参数已经能解析成 JSON，因此以后增量全部忽略”作为通用规则。合法参数值可能在后续事件变化；是否重复应由事件语义和终态快照决定。

参数上限、pending 项数量、原始事件大小需要可配置保护；超限返回结构化错误，不无限缓存，不在此草案硬编码未经测量的新默认阈值。

### 6.4 各适配器的身份职责

| 适配器 | 定位依据与处理 |
| --- | --- |
| Chat Completions | 保留 choice index + tool-call index；按响应位置绑定 CallKey，无 index 的兼容兜底显式记录；不能把不同 choice 混成一条 assistant 输出 |
| OpenAI Responses | item_id/output_index 映射到 CallKey，处理 added、arguments、output_item.done、completed |
| DeepSeek Responses | 先从 SDK 事件识别输出项，再映射相同语义；保留 DeepSeek 特有字段和 SDK 的私有文本解析 |
| Codex | 尽量复用已验证一致的 Responses 归一化，不复制完整响应收集器；OAuth、WS 与前缀缓存仍为专有实现 |
| Anthropic | content block index 定位；保留 thinking/signature、tool_use 和停止语义，不硬套 Responses 原始事件 |

如果 item_id 与 output_index 指向冲突，不能仅凭 call_id 猜测合并；返回协议诊断并阻止受影响调用执行。丢失所有关联信息的事件无法可靠恢复，应明确报错，不声称任何异常流都能修复。

当前生成契约仍表达一条 assistant 输出；对多 choice 响应维持当前有效支持范围并建立测试。若要支持多候选，应单独扩展契约，不在本次重构顺带实现。

### 6.5 单一 stream 的 JSON 兼容与共享收集

- 所有生成都请求流式；HTTP 设置 `stream=true`，Codex WS 等使用协议对应的流模式。
- 服务端忽略 stream 参数返回 JSON 时，在同一次请求内解析并转为统一快照事件，交共享组装器；不得额外发 complete 请求或默认退回非流式链路。
- 保留完整 JSON 解码器是兼容输入形态，不是保留完整响应生成 API；新增适配器只实现 stream。
- 不把大 JSON 人工拆成字符串 delta；按内容块/工具项构造快照，分别保留 CallKey、usage 和终态。
- SSE、适用的 WS 与 JSON 合成流最终比较语义，不要求响应时间、分块数或占位 ID 字面相同。
- usage 更新的累计/增量口径由适配器明确；不能对所有字段一律求和或取最大值。
- collector 必须读取协议要求的终态/用量；不能收到第一段文本就成功，也不能在 usage 可能随后到达时无条件提前关闭。
- 明确只支持非流式的端点返回能力错误；不增加隐藏 fallback。网关已经返回 JSON 的兼容解析不等于主动改用非流式请求。

单一 stream 是统一生成方法和事件语义，不是统一原始 decoder；各协议仍保留必要解码差异。迁移测试使用原非流式结果作参考，不因此保留生产中的非流式生成方法。

## 7. SDK 与适配器收敛

### 7.1 保留差异，删除重复机制

每个适配器原则上只包含：

1. 统一请求到 SDK 请求的映射。
2. SDK 流事件到统一事件的映射，以及流请求收到完整 JSON 时的兼容事件合成。
3. 原始错误到统一结构化错误的转换。
4. 确有需要的协议方言兼容，不包含业务判断。

不要求每个适配器都生成 config/client/error/mapping/provider/stream 六种文件。按真实规模组织，不为了模板增加空壳。

每个适配器只对外实现 stream；标题、对话和完整结果收集共享此路径，不各实现一份 complete/collect。

### 7.2 SDK 具体处理

- Anthropic、DeepSeek：继续使用现有独立 SDK，不把统一领域类型反向塞进 SDK。
- OpenAI：保留当前 SDK/原始响应兼容方式；共享 HTTP/SSE 设施，Chat Completions 与 Responses 的语义 decoder 分开。
- DeepSeek 与 OpenAI Responses：只共享经过契约测试证明一致的归一化状态，不直接把两家原始类型视为相同。
- Codex：复用 Responses 语义模块；认证、凭据和 WebSocket 连接管理在集成层专有模块中维护。
- 文本协议兜底如 DeepSeek DSML 继续留在对应 SDK，不上移到 Core 或公共生成模型。

### 7.3 client 生命周期与资源

- 客户端按端点/凭据配置构建并复用连接池，不每次请求重新创建所有 HTTP client。
- 配置更新后创建新实例，旧实例允许已发请求按既有取消规则结束；不全局静态缓存凭据。
- cache 路由键和 Codex 会话连接归属显式传入，不因复用 client 把不同会话串在一起。
- 丢弃流/取消 future 应停止本次读取；WS 必要时保留共享连接，但必须取消或隔离该请求，防止残留帧进入下一请求。
- 此部分是高风险实现，必须通过真实本地阻塞 I/O 和连接复用测试，不只用内存事件迭代器验证。

## 8. 执行策略、错误与观测

### 8.1 策略不进入纯领域契约

共享执行器放在集成层，作为生成 client 的包装，而不是每个 provider 重写。底层适配器提供单次尝试，上层执行器负责共同策略；SDK 内无法关闭的重试必须记录并纳入预算，不能假装只有一层。

拆分：

- GenerationOptions：模型生成参数。
- ExecutionOptions：连接/响应等待、读空闲、总截止时间、重试预算和取消。
- Observation：追踪、重试通知、计时和 UI 桥接。

### 8.2 超时和取消

- 区分建立连接、等待响应头、读取空闲和整个逻辑调用截止时间。
- 长时间持续有输出的 SSE 不应误用普通完整响应超时截断。
- 重试等待计入总预算；总截止时间耗尽立即结束，不再开始下一次尝试。
- 取消需覆盖网络请求、流读取、退避 sleep、WS 请求等待；不后台偷偷继续生成。
- 迁移初期保持原 timeout 配置语义；更细超时选项先可选启用。改变默认行为单独列为兼容变更，不能夹带在搬代码中。

### 8.3 重试边界

| 情况 | 推荐行为 |
| --- | --- |
| 认证、无效请求、不支持参数 | 不重试 |
| 确定性解码/协议错误 | 不盲目重试，返回诊断 |
| 连接建立失败 | 依现有显式策略有限重试 |
| 限流、明确瞬时服务错误 | 在预算内退避，保留 Retry-After 等元信息 |
| 已开始输出正文、思考、签名或工具数据 | 默认不透明重放；返回 partial，中断恢复由上层决定 |
| 尚未向调用方暴露输出的瞬时中断 | 依策略可重试；不能保证上游未计费 |

当前实现允许“只有思考输出后仍重试”；本方案建议保守地把已对外暴露的思考/签名也作为输出提交边界。**这是行为变化，不在第一阶段默认开启，需在 P0/P4 评审并测试。** 兼容模式应保留旧口径，直到业务事件可以明确分隔/重置 attempt。

建立阶段重试和流中断重试共享预算，不允许各自固定次数产生乘法放大。初期先记录实际尝试次数并保持默认行为，预算收敛单独验证和批准。

现有连接失败改直连逻辑属于网络策略，不能在抽公共 helper 时扩展成所有错误都绕过代理；保留原适用范围，是否允许直连应由网络配置控制。

### 8.4 错误模型

最少需要表达错误类别、发生阶段、是否可能重试、上游状态/代码、可选 Retry-After；不能仅靠错误文案包含 overload 或 retry 判断全部情况。

错误类别建议包括配置、认证、无效请求、能力不支持、限流、传输、超时、协议/解码、服务端错误和取消。`Stream` 不是一个足够精确的可重试类别。

原始 SDK 错误只在边界内保留；对上层暴露稳定错误。部分响应由收集结果携带，避免把 Interrupted 返回为没有任何标识的正常成功。

### 8.5 观测与敏感数据

- 默认日志只记录协议、模型、耗时、尝试次数、错误类别、输出数量和身份冲突数量。
- 不默认记录 tool arguments、提示词、完整响应、缓存键、Authorization 或凭据。
- 诊断采样必须显式启用、限量并脱敏，不以“排查流问题”为由扩散原始会话。
- TTFT、decode、思考耗时和近似 token 计数沿用已定义口径；官方 usage 与近似值分开标注。
- 如果 TTFT 包含 SDK 内部重试，必须明确端到端口径；不能迁移后仍沿用旧名字却从更晚的时刻开始计时。

## 9. 非生成能力的处理

| 当前能力 | 目标处理 | 防回归要求 |
| --- | --- | --- |
| list_models / model infos | 集成层独立 `ModelCatalog` | 保留显示名、上下文窗口、服务端顺序和去重规则 |
| embedding / rerank | 集成层独立 trait 和工厂 | 记忆插件、本地模型实现继续可接入，不依赖生成 trait |
| OAuth 登录与凭据 | 集成层 auth 模块/已有宿主认证服务 | 生成核心不读写凭据；保留刷新与错误恢复 |
| ModelEndpoint / ModelsConfig | 集成层配置模块，业务路由由上层决定 | 序列化键、别名、默认值和现有配置文件保持兼容 |
| 标题、改写、摘要等简单生成 | 轻量请求经 stream + 公共 collector | 不强制创建 Session；业务后处理与是否写入由调用方决定 |
| complete_text helpers | 临时转发 stream + collector，迁移完删除旧入口 | 记忆写入/召回保留用量；不得另起 complete 分支 |
| 同步 CLI 调用 | 阻塞桥接，远离异步核心 | 避免在已有 Tokio runtime 内重复 block_on/panic |

不因“LLM 只负责生成”就删除这些现有产品能力。它们只是离开生成契约，不是从项目中移除。

## 10. 迁移影响与兼容策略

### 10.1 调用方范围

已定位的主要调用位置：

- [Core 请求组装](../crates/tiangong-core/src/context/model_request.rs)：转换实现留在 Core，接入 Session 输入适配；业务流程直接调用 client.stream(&session, options)，不显式维护中间转换链。
- [Core 压缩](../crates/tiangong-core/src/context/compressor.rs)：显式保留 token 预算、tool choice 和思考选项。
- [Core runtime](../crates/tiangong-core/src/runtime.rs)、[turn context](../crates/tiangong-core/src/turn_context.rs)、[Core 构建](../crates/tiangong-core/src/core/mod.rs)：使用带输入转换的 client facade；内部唯一 stream + collector/observer，返回 Core 可消费的数据与事件。
- [ReAct 工具记录](../crates/tiangong-core/src/react/execute.rs)、[Session 唯一化](../crates/tiangong-core/src/session.rs)：保持 ID 职责不变；业务工具校验迁入时建立独立回归。
- [桌面命令](../src-tauri/src/commands.rs)、[Web 配置命令](../src-tauri/src/web_config/commands.rs)：切换模型目录入口，不改变外部命令响应。
- [记忆插件 writer](../plugins/tiangong-plugin-memory/core/src/writer.rs)、[store](../plugins/tiangong-plugin-memory/core/src/store.rs)、[本地模型适配](../plugins/tiangong-plugin-memory/core/src/local_model/provider.rs)：保留文本、embedding、rerank 接口能力。
- CLI/Server 及其他使用方在 P0 用全仓检索补齐清单；以上不是声称已穷尽全部引用。
- 标题生成、纯文本 helper 列为专门迁移项；不为复用对话入口创建临时 Session。

### 10.2 分两种兼容，避免循环依赖

1. **同 crate 过渡期**：增加单一 stream 实现；旧 `SingleProviderClient`/`ModelClient` 和 complete_text 只转发 stream + collector。旧 complete 签名可以短暂兼容，不得保留非流式请求执行。
2. **拆 crate 时**：批量更新工作区 import 和装配入口，将兼容包装一并移入 adapters。纯 `tiangong-llm` 不反向 re-export adapters，否则形成循环依赖。

这是内部 Rust 源码 API 迁移，允许在同一受控阶段更新工作区调用方；不承诺外部 Rust 使用者零 import 改动。若存在工作区外消费者，应先公布迁移说明和版本策略。

配置文件、会话存储、GUI/Server/插件协议不因 Rust import 变化而迁移。过渡 API 必须有删除条件，调用方迁移完后删除旧入口，不永久维护新旧两条执行路径。

### 10.3 请求与响应等价性

- 迁移前录制本地 mock 请求体快照，覆盖当前五种适配器和实际有差异的生成选项。
- 从旧非流式调用迁移时，`stream` 标志和协议要求的流式 usage 选项是预期差异；白名单逐项比对，不声称请求逐字节不变。其余提示词、工具顺序和预算保持等价。
- 确定性请求 JSON 比较结构、数组顺序和字段缺省；签名/缓存依赖精确序列化时再比较字节。
- 动态追踪 ID、时间戳等单独断言存在与关联关系，不能为了快照剔除业务相关字段。
- 响应比较文本、工具顺序/数量/参数、思考/签名、usage、停止原因与状态；不把流分块差异当成失败。
- 已知有损行为是刻意修复项，不要求保留“只剩第二个调用”等错误结果，必须在对照中明确列出。

### 10.4 旧历史配对修正

当前请求侧 `sanitize_tool_call_pairing` 不用于恢复流解析丢失。第一轮迁移保持既有行为与测试，不扩展为历史 ID 重写。

后续若调整其放置位置或把静默过滤改为严格错误，应单独列出兼容变更；不能在这次职责拆分中顺带改变旧会话发送行为。

## 11. 分阶段实施与交付门槛

代码阶段均**未开始，仍需实施授权**；单一 stream、Session/轻量请求输入和调用方输出适配的设计方向已确认。每阶段独立审查，不把方向确认写成实现完成。

| 阶段 | 工作内容 | 依赖 | 可验证完成标准 |
| --- | --- | --- | --- |
| P0：冻结契约与基线 | 补齐标题等直接调用；确定单一 stream、Session/轻量输入、Core 输出与 collector 的编译样例；固定错误/终态/执行选项；快照与异常场景 | 实施授权 | 输入适配和对象安全样例通过；新旧流标志差异明确；异常可稳定复现 |
| P1：修复响应内身份 | 新 CallKey；更新协议事件映射；覆盖 JSON 合成流；稳定补全元数据 | P0 身份契约 | 原重复 ID/ID 后到问题不再覆盖、串扰；Core ID 逻辑不变 |
| P2：统一流消费与响应组装 | 共享增量/快照/终态状态机；collector/text collector/observer 复用；替换 Codex 重复收集；无效候选 | P1 | 实时消费和收集结果等价；中断不伪成功；事件、签名、usage、顺序不回归 |
| P3：统一 stream 与类型适配 | 演进现有 trait；合并内部双模型；Session/轻量输入和 Core 输出转换；迁移标题/文本任务及校验/观测；旧 complete 转发 stream 后删除 | P2、P0 API 定稿 | 所有生成仅走 stream；标题不建 Session；Core 直接消费结果；无非流式 SDK 请求与重复收集器 |
| P4：共享执行策略 | 单次尝试边界、共同预算、超时、取消、错误分类、client 复用 | P3，可提前做只读盘点 | 实际网络尝试数可断言；取消/超时/部分输出重试测试通过；默认变化单独获批 |
| P5：物理拆分与非生成迁移 | 新 adapters crate；搬 SDK/配置/catalog/auth/embedding/rerank；删除重复 dispatch | P3/P4 | 依赖无环；纯生成 crate 无 SDK/transport/业务类型依赖；所有工作区消费方编译 |
| P6：清理与文档同步 | 删除旧包装；更新架构约束、调用说明和测试命令；最终依赖审查 | P5 | 无遗留同义 API；能力保持；全量相关检查通过，已知风险明确 |

P1/P2 可以作为独立正确性修复先交付，不必等待所有 API/目录迁移。P1 的失败场景回归与修复应一并合入，不将红色测试提交到公共主线。

不提供未经实现验证的工期和减行百分比。P0 结束后依据调用方数量、快照覆盖和 WS 资源测试结果估算工作量。

## 12. 测试与验收方案

### 12.1 正式测试矩阵

不只把临时探针原样搬进仓库，而是提取最小、可读、可维护的 fixtures。并非每个协议都有完全相同的原始事件；不适用项明确注明原因。

| 类别 | 必测场景 | 断言 |
| --- | --- | --- |
| 正常生成 | 正文、纯工具、多工具、正文+工具、思考+正文 | 内容、顺序、usage、停止原因完整 |
| 流式输入形态 | SSE、适用的 WS、stream=true 收到 JSON | 等价调用不丢失；JSON 兼容不额外发请求 |
| 输入适配 | 借用 Session + 本轮选项、标题轻量请求、转换失败 | Core 无 SDK 分支，简单任务不创建/写入 Session，错误明确 |
| 消费方式 | 实时 observer、完整 collector、text collector、Core 输出转换 | 同一流只读一次，内容和元信息等价，不再发 complete |
| 标题等简单任务 | 正常短文本、中断、截断、空内容、意外工具、usage 尾部 | 严格识别终态；业务清洗/回退留在调用方，不静默接受 partial |
| 重复 ID | 不同 index/item 共用 call_id、同名称同参数、占位 ID 与上游 ID 同值 | 独立调用全部保留，不能按字符串覆盖 |
| 事件缺失/乱序 | completed-only、done-only 后 completed、delta-before-start、ID 后到 | 可恢复时正确恢复，无法恢复时明确诊断 |
| 重复事件 | 重复 added、done、completed | 幂等，不重复拼参数/输出 |
| 参数快照 | delta+snapshot、冲突快照、合法空对象、缺失参数、非对象 JSON、截断 JSON | 类型/完整性明确，不把坏调用变成合法 `{}` |
| 协议结束 | finish_reason、completed/incomplete、合法 DONE、意外 EOF | 正确 Complete/Incomplete/Interrupted |
| 错误 | 401/400/429/5xx、连接失败、流内错误、解码错误 | 分类与可重试判断一致 |
| 重试 | 建立失败+流中断、Retry-After、耗尽预算、已输出思考/正文/工具后失败 | 总次数和可见产出不重复 |
| 生命周期 | 阻塞 HTTP 读取、退避取消、WS 取消/断线/复用、多会话隔离 | 无残留请求读取、无跨请求串帧 |
| 元信息 | cache usage、思考签名、redacted thinking、模型目录元信息 | 既有协议必需数据不丢失 |
| 上下文与工具 | Core 参数校验、未知工具、权限失败、Core 同批/跨轮 ID 冲突 | 不越层执行，不改历史 |
| 非生成消费者 | 记忆文本 helper、embedding/rerank、本地模型、桌面模型列表 | import 迁移后能力不回归 |

前述 26 个探针场景应全部有正式对应覆盖；其中旧非流式对照转为固定语义基线及 stream + collector 等价断言，不为保留测试而保留生产非流式执行器。预期是修复后满足语义断言，不把 `preserved=false` 固定为成功结果。

### 12.2 验证命令

以下是未来实施时的验证建议，**不是本次已执行的结果**。依实际改动范围和项目 CI 调整，不能只跑这些命令就声称全部集成通过。

```sh
cargo fmt --all -- --check
cargo check --locked -p tiangong-llm -p tiangong-core
cargo test --locked -p tiangong-llm -p tiangong-anthropic -p tiangong-deepseek
cargo test --locked -p tiangong-core
cargo clippy --locked -p tiangong-llm -p tiangong-core --all-targets -- -D warnings
git diff --check
```

新增 adapters crate 后把它加入上述检查；同步验证 CLI/Server/App 和记忆插件各自 manifest/CI 规定的构建范围。独立 workspace 或平台专属 target 按各自规则验证，不假设一次 `--workspace` 覆盖全部。

真实 API smoke 需要凭据、网络及可能的费用授权；本地 mock 是合入前必要项，真实 smoke 是补充，不用真实调用代替确定性异常测试。没有凭据时明确标记未测，不伪称 SDK 全链路线上稳定。

### 12.3 最终验收清单

- [ ] 一个权威生成 trait，仅有 stream 生成方法；旧 complete/helper 只可临时包装 stream，最终删除旧入口。
- [ ] Session 与轻量请求经转换接入；标题不创建/写入 Session，Core 不显式操作 SDK 请求。
- [ ] Core 直接获得本地可消费的结果/事件；输入输出转换不自动修改会话或执行工具。
- [ ] 完整、文本收集和实时观察复用同一组装器；中断/截断不被 collect_text 当作成功。
- [ ] Core 不依赖 SDK 类型，不执行 LLM 内部的协议分支。
- [ ] `tiangong-llm` 不依赖各 SDK、HTTP/WS、Core 或业务 `TokenUsage` 类型。
- [ ] 不再存在多份完整的重试日志退避循环或工具流收集器。
- [ ] SSE、适用的 WS 与 JSON 合成流均保留独立输出项；JSON 兼容不触发非流式重发。
- [ ] ID 后到、缺失中间事件、增量+快照有确定处理，无静默覆盖。
- [ ] 无效/中断调用不会作为正常完整调用执行。
- [ ] 同一工具名称和参数的合法多次调用全部保留。
- [ ] 正文、思考、签名、usage、停止原因和顺序无回归。
- [ ] 实际重试次数、取消和超时有真实本地 I/O 证据。
- [ ] Core 唯一化职责未变，无历史落盘或请求 ID 迁移。
- [ ] 配置键、协议别名和上层 IPC/HTTP 兼容，非生成服务可用。
- [ ] 现有架构文档已在批准实施后同步；风险和未测部分如实记录。

## 13. 风险与回退

| 风险 | 影响 | 降低方式 / 回退边界 |
| --- | --- | --- |
| 双请求模型合并改变默认值 | 思考、温度、token 上限或缓存失效 | 先冻结请求快照；兼容入口显式解析旧默认；按阶段回退映射 |
| 简单任务由非流式迁到流式 | 端点不支持、usage 缺失、流中断使标题不完整 | 验证端点能力/尾部 usage/严格 collector；不静默 fallback；提示词和预算变化显式列出 |
| 转换入口隐含会话生命周期 | 复制整个 Session、跨网络持锁、转换失败被吞 | Core 本地适配，准备后释放借用；编译与取消测试；错误保留 |
| 统一 Responses 状态机误伤方言 | DeepSeek/Codex 事件或参数语义变化 | 共享语义不共享未经证明的原始字段；独立 fixtures |
| 工具校验移到 Core | 原有恢复链路失效或坏调用被执行 | 先桥接旧 InvalidToolCall；迁移校验与执行门禁同批测试 |
| 重试收敛改变体验/请求量 | 失败率、等待时间、重复生成或费用变化 | 新旧策略对照，保留显式兼容模式，不隐式改变默认 |
| 拆 crate 出现循环依赖 | 无法编译、纯接口反向依赖实现 | 先画依赖和迁移 import；不从 llm re-export adapters |
| 连接池/WS 复用混淆会话 | 缓存串扰、帧串入下一请求 | 显式请求归属、真实取消/复用测试；可先保持原生命周期 |
| 终态不完整被当成功 | 执行截断参数 | 显式 status 和单调用完整性门禁 |
| 日志增加敏感输出 | 参数、提示词、凭据泄露 | 只记计数和分类，诊断显式启用与脱敏 |

回退原则：

1. 每阶段独立、可审查，保留行为快照和契约测试。
2. 纯内部 API/模块迁移可以通过正常 revert 回退；不需要会话数据回滚。
3. 已确认的丢调用修复尽量独立于大规模迁移，避免因目录回退而再次恢复有损解析。
4. 不设置长期新旧双执行路径。过渡模式有明确删除条件，不成为永久 feature 开关矩阵。
5. 外部协议/配置若确需变更，另起兼容设计，不借本次方案实施。

## 14. 已确认方向与待评审决策

以下方向已由讨论确认，尚不代表授权实施：

- 生成只保留 stream；一次性结果由 collector 获得，不再独立调用 complete。
- 对话支持直接传入借用的 Session 和本轮选项，转换逻辑仍由 Core 实现。
- 标题等简单任务直接提交轻量请求，不创建 Session，也不必展示增量。
- 输出经转换返回 Core 可处理的数据；流式工具调用的正确性与 Core 唯一化边界不变。
- 保留 stream=true 收到 JSON 的兼容解析，不把它发展为主动非流式回退。

下表是其余实施评估项和建议默认，不要求逐个命名细节审批。

| 决策 | 推荐默认 | 为什么 |
| --- | --- | --- |
| 是否采用单次生成边界 | 是 | 与 Core 职责分离，消除双模型和多入口 |
| 是否保留薄协议适配器 | 是 | 差异真实存在，强行消除会变成大分支实现 |
| 是否分离响应内身份与 call_id | 是，优先实施 | 已有确定性丢调用证据 |
| 是否增加一个集成 crate | 是，在 P5 进行 | 真正隔离依赖，不一开始大搬家 |
| 是否把 SDK 全部重写 | 否 | 不解决核心身份问题，增加不必要风险 |
| 是否统一所有原始协议解析器 | 否 | 统一语义状态，保留协议/方言解码 |
| 是否把工具 schema/权限校验留在 LLM | 否，移到 Core/工具层 | 是执行安全与业务职责，不应只对部分协议生效 |
| 是否改变已有输出后的自动重试 | 建议更保守，但单独批准 | 防重复输出；会改变现有思考重试体验 |
| 是否同时更改默认超时/重试次数 | 否 | 先收敛结构，行为变化独立测量与评审 |
| 是否现在同步现有架构规范 | 草案期不替换，批准后同步 | 避免建议被误当作当前强制规则 |
| 是否处理旧会话/历史 ID | 否 | 明确不在本次范围 |

**推荐下一步：基于已确认方向决定是否授权 P0–P2，再根据 API 编译样例、调用方清单和测试结果确认 P3–P6。** 这样尽早修复正确性问题，同时控制 API 与目录迁移范围。

## 15. 本文交付说明

- 本轮仅按已确认讨论修订设计文档，不是代码实施授权，也不代表问题已修复。
- 初版已核对现场源码和历史探针统计；本轮检查文档及 Git 范围，不重跑探针、不修改生产代码、不调用真实 API。
- Coding 工作流工具在本环境报告内部“未知的 Coding 操作”，未能提供自动上下文/预检；采用直接读取源码、Git 和文档结构检查补充核验。自动审查若同样失败，应如实披露，不记为通过。
- 后续实施采用本方案批准的阶段范围，再建立实际 PLAN/TODO 或遵循当时仓库流程；本次不为评估文档额外创建开发任务文件。
