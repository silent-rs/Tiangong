# tiangong-llm 架构约束

## 职责与本轮范围

`tiangong-llm` 是单次模型生成与 SDK 之间的统一边界。生成仅使用 `LlmProvider::stream`；一次性结果由同一流的 collector 获取，不维护独立非流式生成执行器。

**本轮按用户决定保持适配器在现有 llm crate，不新增 adapters crate。** SDK、配置、OAuth、HTTP/SSE/WS、模型目录、embedding/rerank 暂保留原模块位置，但必须与生成契约和会话业务区分职责。

完整设计及实现差异见 [单次流式生成重构方案](tiangong-llm-refactoring-proposal.md)，尤其第 15 节的兼容范围和未扩展项。

## 依赖与输入输出

- Core 可以通过 client facade 直接传 `&Session` 和本轮选项；输入转换由 Core 实现 `IntoModelInput`，LLM 不依赖 Core、不读取 Session 或本地附件文件。
- 标题、改写等简单任务用 `TextRequest`，不创建临时 Session；是否写回由业务调用方决定。
- `Generation` 完成输入准备后拥有请求，不跨网络等待持有会话锁；输出可通过 TryFrom 转成 Core 本地结果。
- `ModelRequest`/`ProviderRequest`、旧同步 helper 暂作兼容，均走同一 stream + 组装器。删除兼容 API 或合并字段须覆盖实际消费者，不能新增同义执行路径。
- 第三方 SDK 类型不暴露给 Core，协议配置只在装配/映射处使用。

## 当前协议

当前包含 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、DeepSeek 原生 Responses、Codex Responses/WS。协议原始 decoder 保留差异；同族可共享经验证一致的语义处理，不因名称相同就强行合并原始字段。

Anthropic、DeepSeek 继续使用独立原生 SDK；私有文本工具协议兜底留在所属 SDK。Codex 认证和会话连接管理保持专有逻辑，不回落 HTTP。

## 流式正确性

- 响应内工具调用按 `CallKey` 关联；上游 call_id 只是配对元数据，允许不同输出项重复，不作为聚合主键。
- Metadata 更新不得清空参数。Delta 与完整 Snapshot 区分；终态快照定稿，重复相同快照幂等，冲突调用显式无效。
- completed-only、参数先于名称、ID 后到、缺失部分中间事件，必须在仍有 item/index 身份信息的层处理，不能依赖 Core 恢复已丢失调用。
- SSE 请求收到完整 JSON 时在同一次请求内合成统一流；保留解码器不等于保留主动非流式路径。
- collector 保留正文、工具、思考/签名、redacted thinking、usage 和完成状态。中断/截断不能冒充完整成功。
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
