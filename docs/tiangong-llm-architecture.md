# tiangong-llm 架构约束

## 职责与本轮范围

`tiangong-llm` 是单次模型生成与 SDK 之间的统一边界。生成仅使用 `LlmProvider::stream`；一次性结果由同一流的 collector 获取，不维护独立非流式生成执行器。

**本轮按用户决定保持适配器在现有 llm crate，不新增 adapters crate。** SDK、配置、OAuth、HTTP/SSE/WS、模型目录、embedding/rerank 暂保留原模块位置，但必须与生成契约和会话业务区分职责。

完整设计及实现差异见 [单次流式生成重构方案](tiangong-llm-refactoring-proposal.md)，尤其第 15 节的兼容范围和未扩展项。

## 依赖与输入输出

- 对外生成入口为配置好的 `SingleProviderClient::stream(Vec<tiangong_types::Message>)`，不接收 Session，不接收第二个 options 参数。
- `Options` 在 client 构建/派生时设置，包含思考、输出预算、tools/tool_choice、缓存键、timeout 和重试次数；配置不可变，派生 client 不改变原实例，共享底层连接资源。
- Core 的 `TurnContext` 持有配置好的 client；构建时及下一次正式 ReAct 请求前同步运行中的工具/思考配置。请求前压缩先使用现有快照，仅覆盖 `max_output_tokens`。如果运行中切换 `ReasoningEffort`，压缩仍沿用旧档位以尽量复用服务端 KV 缓存，新档位在其后的正式 ReAct 请求中生效。压缩和标题均使用独立快照，不污染主请求。
- `Session::context()` 保持原有消息选择语义：系统提示 + 摘要边界后的消息，排除持久化 System/Notice，保留其余顺序和数量。仅通过 Core 私有的单图片处理补齐历史图片/失败降级，不过滤前置消息、不插入兜底、不写回历史。
- 不提供公开通用 `prepare_messages`/`prepare_context` 层。前置非 User 消息沿用 LLM 现有协议请求清理；无有效 User 的旧恢复策略只在 Core 主请求发起时追加，不属于 context()。附件分析构造 Image 时自行读入数据，不依赖 Core 准备接口。
- LLM stream 内部只做消息角色/内容的纯映射与 options 装配，直接生成 `ProviderRequest`。未准备的本地图片明确报错，不读文件。
- 标题/摘要指令由 Core/业务模块构造为消息。简单任务不创建 Session；文本 helper 仅配置 client 并调用相同 stream/collector，不自行构造 provider 请求。
- `Generation` 拥有输入和 client 快照，不跨网络借用 Core；输出由 Core 实现 TryFrom/From，仍由 Core 校验和执行工具。
- 旧 `ModelRequest`/同步执行桥仅用于旧回归测试，不提供生产执行入口；生产 ReAct、压缩、附件分析和文本 helper 均不使用它们。消息转换与 SDK 规范化请求不再经过双请求模型。
- `ProviderRequest` 为 LLM/适配器内部规范化边界；Core 生产路径不构造该结构，不暴露第三方 SDK 类型。

## 当前协议

当前包含 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、DeepSeek 原生 Responses、Codex Responses/WS。协议原始 decoder 保留差异；同族可共享经验证一致的语义处理，不因名称相同就强行合并原始字段。

Anthropic、DeepSeek 继续使用独立原生 SDK；私有文本工具协议兜底留在所属 SDK。Codex 认证和会话连接管理保持专有逻辑，不回落 HTTP。

## 流式正确性

- 响应内工具调用按 `CallKey` 关联；上游 call_id 只是配对元数据，允许不同输出项重复，不作为聚合主键。
- Metadata 更新不得清空参数。Delta 与完整 Snapshot 区分；终态快照定稿，重复相同快照幂等，冲突调用显式无效。
- completed-only、参数先于名称、ID 后到、缺失部分中间事件，必须在仍有 item/index 身份信息的层处理，不能依赖 Core 恢复已丢失调用。
- SSE 请求收到完整 JSON 时在同一次请求内合成统一流；保留解码器不等于保留主动非流式路径。
- collector 保留正文、工具、思考/签名、redacted thinking、usage 和完成状态。相较于 #551 中“保留已产出内容且不让整轮失败”的行为，ReAct 的纯文本分支仍持久化部分正文和用量，但不把中断或截断当作正常完成。压缩不提交非 `Complete` 的摘要（输出达到 `MaxTokens` 的拒绝逻辑原已存在）。
- Core 保持新增工具调用同批/跨轮唯一化；不改历史、不做旧会话迁移、不在请求副本重写历史 ID。

## 执行与业务边界

- 公共重试执行器统一有限预算、退避和通知；确定性解码/请求错误不盲重试。原代理连接失败直连策略不得扩大。
- 请求取消应终止读取和退避；WS 仅 clean 终态归还连接，不完整请求丢弃连接，避免下一次串帧。
- provider/HTTP client 可以复用，但缓存路由、timeout 和配置不能跨会话混淆。
- 工具 schema、授权、实际执行、结果回填和多轮循环由 Core/工具运行时负责。
- 模型目录使用独立 `ModelCatalog`；embedding/rerank 不加入生成 trait。
- 默认日志不打印提示词、工具参数、响应原文、缓存键或凭据；诊断需显式启用且受控。

## 新增和修改适配器

1. 先核对现有生成契约，不增加独立 complete 方法。
2. 仅实现请求映射、原始流到统一事件映射、JSON 兼容和错误转换。
3. 复用公共组装器和执行器，不复制工具流收集/重试循环。
4. 添加跨协议契约和本地 HTTP/WS 回归，覆盖正常、失败和缺失事件。
5. 若需物理拆 crate、变更默认重试/超时或外部配置，另行评估授权，不夹带进内部整理。
