# tiangong-deepseek

An asynchronous Rust client for the [DeepSeek API](https://api-docs.deepseek.com/).

## Features

- **Chat Completions** — synchronous and SSE streaming, with tool calling and reasoning support
- **Thinking Mode** — `thinking`（enabled/disabled）+ `reasoning_effort`（low/high/max）分档控制，`reasoning_content` 思考内容解析
- **Text Protocol Fallback** — 内置工具调用文本协议兜底（原生 + DSML），SDK 自动从 `content` 文本中识别并解析
- **Streaming Robustness** — 单 chunk 多事件收集、空 delta 容错
- **Finish Reason** — 普通响应与流式事件均原样保留服务端结束原因，支持完整 JSON 回退和未知值
- **Responses API** — OpenAI Responses 兼容格式（适配 Codex 场景），非流式 + 流式，支持图片输入（`image_url` / `file_id`）、function / web_search 工具、reasoning effort
- **Files API** — 图片文件上传（multipart，支持有效期）、列出、查询、删除，`file_id` 可在对话中引用
- **Search / Fetch API** — 网页搜索（结构化结果、搜索语法、可选正文）与网页抓取（Markdown 正文），本地参数校验
- **Model Listing** — list available models
- **Balance Queries** — check account balance (CNY / USD)
- **Context Caching** — `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` exposed in usage
- **Retryable Errors** — `DeepSeekError::is_retryable()` for transport and rate-limit failures

## Quick Start

```toml
[dependencies]
tiangong-deepseek = "0.1.6"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
futures-util = "0.3"
serde_json = "1"
```

```rust
use tiangong_deepseek::{DeepSeekClient, types::*};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::builder("your-api-key").build()?;

    // Chat completion
    let response = client.chat().create(ChatCompletionRequest {
        model: "deepseek-v4-pro".into(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!("Hello!")),
            ..Default::default()
        }],
        ..Default::default()
    }).await?;
    println!("{}", response.choices[0].message.content.as_deref().unwrap_or_default());

    // Streaming with thinking mode + usage
    let stream = client.chat().create_stream(ChatCompletionRequest {
        model: "deepseek-v4-pro".into(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!("Explain Rust in one sentence.")),
            ..Default::default()
        }],
        stream: Some(true),
        stream_options: Some(StreamOptions { include_usage: true }),
        thinking: Some(ThinkingConfig { thinking_type: "enabled".into() }),
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    }).await?;

    use futures_util::StreamExt;
    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::ReasoningDelta(text) => eprint!("\r[思考] {text}"),
            StreamEvent::TextDelta(text) => print!("{text}"),
            StreamEvent::TextProtocolToolCall { name, arguments, .. } => {
                println!("\n[文本协议工具调用] {name}: {arguments}");
            }
            StreamEvent::Usage(usage) => {
                let hit = usage.prompt_cache_hit_tokens.unwrap_or(0);
                println!("\n[kv cache 命中 {hit} tokens]");
            }
            StreamEvent::FinishReason(reason) => eprintln!("\n[结束原因] {reason}"),
            StreamEvent::Done => {
                println!();
                break;
            }
            _ => {}
        }
    }

    Ok(())
}
```

> `ChatMessage` fields like `name`, `tool_calls`, `tool_call_id`, and `prefix` default to `None`/`false` via `#[serde(default)]`. You can use `..Default::default()` to omit them.
>
> 当前模型为 `deepseek-v4-pro`（满配）与 `deepseek-v4-flash`（轻量），两者均支持思考模式与工具调用。

## Chat 结束原因（0.1.4）

`finish_reason` 是 [DeepSeek Chat Completions 接口](https://api-docs.deepseek.com/zh-cn/api/create-chat-completion/) 返回的原始字段。

- 普通响应从 `response.choices[index].finish_reason` 读取。
- 流式响应通过 `StreamEvent::FinishReason(String)` 提供；中间块的 `null` 不产生此事件。
- 标准 SSE、漏标内容类型的 SSE、网关返回完整 JSON，以及工具文本缓冲路径都保留该值。
- `StreamEvent::Done` 表示流结束，不能代替结束原因；未知值同样原样透传。

| 服务端值 | 含义 |
| --- | --- |
| `stop` | 自然结束或命中停止字符串 |
| `length` | 达到输出或上下文长度限制，内容可能截断 |
| `tool_calls` | 请求调用工具 |
| `content_filter` | 内容被过滤 |
| `insufficient_system_resource` | 推理资源不足，生成中断 |

SDK 不替调用方判断结果是否完整、是否接受或重试。升级到 0.1.4 时，若原有代码对 `StreamEvent` 使用穷尽匹配，需要增加 `FinishReason` 分支。相关测试覆盖 48 组响应格式、结束原因和工具缓冲组合。

## Responses API

DeepSeek 原生支持 OpenAI Responses API 格式（为适配 Codex 等编码场景推出），可用模型为 `deepseek-v4-pro` / `deepseek-v4-flash` / `deepseek-v4-flash-vision-exp`（`types` 模块导出了对应常量）。

该 API 为**无状态**设计：服务端不存储响应与会话，多轮对话需要客户端在每次请求的 `input` 中回传完整历史；不支持 `previous_response_id` / `store` / `conversation` 等参数（传入会被忽略）。输入超出上下文窗口时服务端直接返回 400。

```rust
use tiangong_deepseek::types::*;

// 非流式：input 可传纯字符串（视作一条 user 消息），instructions 作为首条 system 消息
let response = client.responses().create(CreateResponseRequest {
    model: MODEL_V4_FLASH.into(),
    input: Some(ResponseInput::Text("用一句话解释 Rust".into())),
    instructions: Some("你是简洁的技术助手".into()),
    reasoning: Some(ReasoningConfig { effort: Some(ReasoningEffortLevel::High) }),
    ..Default::default()
}).await?;
println!("{}", response.output_text());  // 拼接全部输出文本（等价官方 response.output_text）
```

```rust
// 流式：事件序列与 OpenAI Responses 兼容，无 [DONE] 标记，
// 以 response.completed / incomplete / failed 结束
let mut stream = client.responses().create_stream(CreateResponseRequest {
    model: MODEL_V4_FLASH.into(),
    input: Some(ResponseInput::Items(vec![ResponseInputItem::Message(InputMessage {
        role: Some(ResponseRole::User),
        content: Some(MessageContent::Text("写一个斐波那契函数".into())),
    })])),
    tools: Some(vec![ResponsesTool::Function(ResponsesFunctionTool {
        name: "run_code".into(),
        description: Some("执行代码".into()),
        parameters: None,
    })]),
    ..Default::default()
}).await?;

use futures_util::StreamExt;
while let Some(event) = stream.next().await {
    match event? {
        ResponsesStreamEvent::ReasoningTextDelta { delta, .. } => eprint!("\r[思考] {delta}"),
        ResponsesStreamEvent::OutputTextDelta { delta, .. } => print!("{delta}"),
        ResponsesStreamEvent::OutputItemDone {
            item: ResponseOutputItem::FunctionCall(call), ..
        } => println!("\n[工具调用] {}: {}", call.name, call.arguments),
        ResponsesStreamEvent::ResponseCompleted { response, .. } => {
            if let Some(usage) = response.usage {
                println!("\n[usage] input {} (cached {}), output {} (reasoning {})",
                    usage.input_tokens,
                    usage.input_tokens_details.map(|d| d.cached_tokens).unwrap_or(0),
                    usage.output_tokens,
                    usage.output_tokens_details.map(|d| d.reasoning_tokens).unwrap_or(0));
            }
        }
        ResponsesStreamEvent::Unknown { event_type } => {
            // 服务端新增而 SDK 尚未支持的事件，保留事件名透传，不中断流
        }
        _ => {}
    }
}
```

### 多轮与工具调用回传

`input` 为输入项列表，支持 `message` / `function_call` / `function_call_output` / `custom_tool_call` / `custom_tool_call_output` / `reasoning` / `web_search_call`。工具调用的配对规则：每个 `function_call` 必须有同 `call_id` 的 `function_call_output`；`web_search_call` 原样回传即可（SDK 保留未知字段），服务端自动恢复搜索结果。

```rust
let history = vec![
    ResponseInputItem::Message(InputMessage {
        role: Some(ResponseRole::User),
        content: Some(MessageContent::Text("北京今天多少度？".into())),
    }),
    ResponseInputItem::FunctionCall(FunctionCallInputItem {
        call_id: "call_0".into(),
        name: "get_weather".into(),
        arguments: "{\"city\": \"北京\"}".into(),
    }),
    ResponseInputItem::FunctionCallOutput(FunctionCallOutputInputItem {
        call_id: "call_0".into(),
        output: FunctionOutputContent::Text("晴，32℃".into()),
    }),
];
```

### 图片输入

`input_image` 内容块支持两种来源，**互斥**（都不传或都传返回 400）：

| 字段 | 说明 |
|------|------|
| `image_url` | http(s) URL（≤8192 字符）或 base64 data URL，支持 JPEG / PNG / GIF / WebP |
| `file_id` | Files API 上传返回的 `file-api-...` ID，不受 32 MiB 内联限制（单张最大 64 MiB），此时 `detail` 被忽略 |

仅 `deepseek-v4-flash-vision-exp` 真正处理图片，其他模型将图片替换为占位文本；图片只能出现在 user / developer 消息及工具输出中，system / assistant 消息含图片返回 400。

```rust
ResponseInputItem::Message(InputMessage {
    role: Some(ResponseRole::User),
    content: Some(MessageContent::Blocks(vec![
        ContentBlock::InputText(TextBlock { text: "描述这张图".into() }),
        ContentBlock::InputImage(InputImageBlock {
            file_id: Some("file-api-0a1b2c3d4e5f60718293a4b5c6d7e8f9".into()),
            detail: Some(ImageDetail::High),
            ..Default::default()
        }),
    ])),
})
```

### 参数支持速览

| 参数 | 说明 |
|------|------|
| `model` / `input` / `instructions` / `stream` | 完整支持；`input` 与 `instructions` 至少传一个 |
| `temperature` / `top_p` | 支持，思考模式下不生效 |
| `max_output_tokens` / `top_logprobs`（0–20）/ `user` | 支持 |
| `tools` / `tool_choice` | function、web_search（含 `web_search_2025_08_26`）；tool_choice 支持 none / auto / required / 指定工具 |
| `reasoning.effort` | `none`–`max` 七档：none 关闭，minimal/low → low，medium/high/xhigh → high，max 最高 |
| `text.format` | text / json_object / json_schema |
| `parallel_tool_calls` / `store` / `previous_response_id` 等 | 不支持，传入被静默忽略 |

## Files API

上传图片文件并在对话中通过 `file_id` 引用，适合多请求复用同一图片或发送超过内联限制的大图。文件归属 API key，可被 Chat Completions 与 Responses API 共同引用。

```rust
// 上传：格式按文件实际内容判断（JPEG/PNG/GIF/WebP），支持设置有效期（3600–2592000 秒，
// 即 1 小时至 30 天），None 表示永久有效
let png = std::fs::read("chart.png")?;
let file = client.files().upload("chart.png", png, Some(7 * 24 * 3600)).await?;
println!("{} -> {} bytes, 过期于 {}", file.id, file.bytes,
    file.expires_at.map(|t| t.to_string()).unwrap_or_else(|| "永久".into()));

// 列出：游标分页
let page = client.files().list(ListFilesParams {
    limit: Some(20),
    order: Some(ListOrder::Desc),
    ..Default::default()
}).await?;
if page.has_more {
    let next = client.files().list(ListFilesParams {
        after: page.last_id,
        ..Default::default()
    }).await?;
}

// 查询 / 删除
let info = client.files().retrieve(&file.id).await?;
let deleted = client.files().delete(&file.id).await?;
assert!(deleted.deleted);
```

限制速览：单文件最大 64 MiB，文件名最长 512 字符，单用户 25 GiB / 10000 个文件，上传须在 10 分钟内完成。

## Search / Fetch API

[网页搜索与抓取](https://api-docs.deepseek.com/zh-cn/guides/search) 为 Agent / RAG 提供实时网页内容，与对话接口共用 Bearer 鉴权。端点固定为 `/v1/tools/search` 与 `/v1/tools/fetch`；`base_url` 配置为 `https://api.deepseek.com` 或 `https://api.deepseek.com/v1` 均可。

```rust
use tiangong_deepseek::types::*;

// 搜索：query 支持 site: / -site: / -词 / "短语" / lang: / after: / before: 语法
let found = client.tools().search(
    SearchRequest::new("DeepSeek 模型 site:github.com OR site:huggingface.co after:2025")
        .count(5)                       // 1–50，默认 10
        .ori_question("DeepSeek 最近发布了什么模型？")
        .need_content(true),            // 返回正文，默认 false
).await?;
for item in found.results.iter().filter(|r| r.is_high_quality()) {
    println!("[{:.1}] {} {}\n{}", item.rerank_score, item.title, item.url, item.summary);
}

// 抓取：返回标题与 Markdown 正文
let page = client.tools().fetch(FetchRequest::new("https://example.com/article")).await?;
println!("{}\n{}", page.title, page.content);
```

结果字段：`rerank_score`（0–3，≥1.5 优质，`is_high_quality()`）、`title`、`url`、`snippet`（短摘要，适合展示）、`summary`（长摘要，适合给模型）、`content`、`site_name` / `site_icon` / `date`（可空）、`authority`（0–4，-1 未知，≥3 极权威，`is_highly_authoritative()`）。

本地校验（不合法时直接返回 `InvalidRequest`，不发请求、不计费）：`query` / `ori_question` 1–128 个 Unicode 字符且非全空白；`count` 1–50；`url` 为含 host 的 HTTP/HTTPS URL、≤8192 字符、不含用户名密码。

错误映射：

| HTTP | `DeepSeekError` | `is_retryable()` |
|------|-----------------|------------------|
| 400 | `InvalidRequest` | 否 |
| 401 | `Authentication` | 否 |
| 402 余额不足 / 403 Fetch 安全策略拒绝 / 404 无可用网页内容 | `Api` | 否 |
| 408 / 5xx | `Transport` | 是 |
| 429 QPS 超限 | `RateLimited` | 是 |

计费与限速：按成功请求次数计费（Search 需 `results` 非空，Fetch 需返回正文）；Search、Fetch 各 5 QPS（账号级），Fetch 另有目标域名级限速。

## Thinking Mode

DeepSeek V4 默认开启思考模式，通过 `reasoning_content` 字段返回思维链。开关与强度控制：

| 字段 | 取值 | 说明 |
|------|------|------|
| `thinking.type` | `"enabled"` / `"disabled"` | 开关思考模式 |
| `reasoning_effort` | `Low` / `High` / `Max` | 思考强度（v4-flash 三档，v4-pro 暂支持 High/Max） |

注意事项：
- 思考模式下 `temperature`/`top_p` 不生效，可不传。
- **多轮工具调用**时，assistant 消息中的 `reasoning_content` 必须随后续请求完整回传，否则 API 返回 400。
- 普通多轮对话（无工具调用）的 `reasoning_content` 无需回传。

## 工具调用文本协议兜底

正常情况下 DeepSeek 通过结构化 `tool_calls` 字段返回工具调用。但部分场景下模型会把工具调用写进 `content` 文本，使用特殊标记协议。**SDK 内置完整兜底**，调用方无需自行处理。

### 触发条件

仅当请求携带 `tools` 时启用。无 tools 时（如总结阶段 `ToolChoice::None`）模型不可能走工具调用，兜底关闭，避免误伤讨论协议的正常文本。

### 工作方式
**流式**（`create_stream`，chat 与 responses 共用同一状态机）：SDK 在接收流式响应的同时缓冲文本，使用三态状态机判定：
- `Idle` → 普通文本即时透传；遇到 `<` 进入探测
- `Probing` → 窗口内（10 字符）匹配协议精确前缀（`｜tool` / `｜｜dsml`）→ `Confirmed`；窗口满未命中 → 整块吐出回 `Idle`（普通文本、代码块零延迟、零丢失）
- `Confirmed` → 协议原文持续缓冲（不作为正文透传，避免泄漏给用户或进入对话历史），流末统一解析：剥离协议块后的可见文本 + 工具调用（chat 产出 `StreamEvent::TextProtocolToolCall`，responses 合成结构化 `function_call` 事件组），`call_id` 取全局唯一计数器
- 流异常中断时缓冲原文冲刷回正文，已接收内容不丢失
- 出现结构化工具调用（`tool_calls` / `function_call` item，含一次性 JSON 合成流在终止事件携带的完整 output）后停用文本协议识别，协议讨论样文本按正文对待

探测使用精确前缀而非宽泛关键词，避免把 `<toolbar>`、`<dsml>` 等普通内容误判为工具调用。

**非流式**：`tiangong_deepseek::dsml::parse_dsml_tool_calls(content)` 从文本提取工具调用，`strip_tool_call_block(content)` 剥离标记文本；`dsml::next_text_call_id()` 生成全局唯一的文本协议调用 id。

### 底层支撑

- **流式 chunk 多事件输出**：单个 SSE chunk 可能同时携带 `reasoning_content`/`content`/`tool_calls`，收集全部而非只取首个非空字段。
- **空 delta 容错**：只含 `role` 的首片和只含 `finish_reason` 的结束片（delta 全空）静默跳过，不报错。

### 已知协议

| 协议 | 前缀标记 | 来源 |
|------|----------|------|
| 原生协议 | `<｜tool▁calls▁begin｜>` 等（竖线 U+FF5C，分隔符 ▁ U+2581） | tokenizer 内置原子 token（id 128806~128814），完整出现 |
| DSML 协议 | `<｜｜DSML｜｜invoke>` 等（双全角竖线） | V3.2 引入的 XML 风格，非原子 token，流式时分片到达 |

原生协议标记是原子 token，模型输出时完整出现，解析采用严格匹配。DSML 协议标记是普通字符流，流式时按字符分片到达，外层包裹可能残缺或完全缺失——采用部分识别策略，只要出现内层 `<｜｜DSML｜｜invoke` 标记就尝试提取工具调用，不依赖外层包裹完整。

## API Coverage

| Endpoint | Method | Status |
|----------|--------|--------|
| `/chat/completions` | POST | Supported (sync + SSE stream, thinking mode, tool calling, text protocol fallback) |
| `/responses` | POST | Supported (sync + SSE stream, images via `image_url`/`file_id`, function/web_search tools, reasoning effort) |
| `/files` | POST | Supported (multipart upload with `expires_after`) |
| `/files` | GET | Supported (cursor pagination: after / limit / order / purpose) |
| `/files/{file_id}` | GET | Supported |
| `/files/{file_id}` | DELETE | Supported |
| `/models` | GET | Supported |
| `/user/balance` | GET | Supported |
| `/v1/tools/search` | POST | Supported (search syntax, count, ori_question, localization, need_content) |
| `/v1/tools/fetch` | POST | Supported (Markdown content) |

## Changelog

### 0.1.6

- **Search / Fetch API**：`client.tools().search()` / `fetch()`，对应 `/v1/tools/search` 与 `/v1/tools/fetch`；`SearchRequest` 链式构造，`SearchResult` 提供优质 / 权威判定方法
- **本地参数校验**：query / ori_question 长度与空白、count 范围、fetch URL 协议 / host / userinfo / 长度
- **错误映射**：tools 接口 403（Fetch 安全策略）不视为鉴权失败，408 视为可重试；兼容 `base_url` 以 `/v1` 结尾

### 0.1.2

- **Responses API**：`client.responses()` 非流式与流式调用（`create` / `create_stream`）；输入项覆盖 message / function_call / function_call_output / custom_tool_call / custom_tool_call_output / reasoning / web_search_call（保留未知字段支持原样回传）；图片 `image_url` / `file_id` 互斥二选一；reasoning effort 七档；流式事件全量解析，终止事件（completed / incomplete / failed）后结束流，未知事件降级为 `Unknown` 透传不中断；`ResponseObject::output_text()` 便捷拼接
- **Files API**：`client.files()` 上传（multipart 手工构造，零新增依赖，`expires_after` 支持 1 小时–30 天有效期）、列出（游标分页）、查询、删除；上传 Content-Type 按文件实际内容嗅探（JPEG / PNG / GIF / WebP）
- **健壮性**：`create()` 强制关闭 `stream`，方法语义与返回类型一致；查询参数与 `file_id` 路径段按 RFC 3986 百分号编码
- **HTTP 层**：client 新增 `get_with_query` / `delete` / `post_multipart` 通用方法
- **测试**：新增 19 项回归测试（请求/响应序列化、SSE 事件解析与未知事件降级、终止事件后停止流、multipart 编码与文件名转义、图片格式嗅探、URL 编码端到端）

### 0.1.1

- **V4 接口适配**：`reasoning_effort` 新增 `Low` 档；思考模式开启时省略 `temperature`/`top_p`；移除未文档化的 `budget_tokens`
- **文本协议兜底**：内置工具调用文本协议解析（原生 `<｜tool▁calls` + DSML `<｜｜DSML`），三态状态机（Idle → Probing → Confirmed），仅 `tools` 请求启用
- **流式健壮性**：单 chunk 多事件收集；空 delta（role 首片 / finish_reason 结束片）静默跳过；`stream_options.include_usage` 自动设置
- **兼容性**：`reasoning_content` 添加 `thinking_content` 别名；`response_format` 经 metadata 透传

### 0.1.0

- 初始发布：Chat Completions（同步 + SSE 流式）、模型列表、余额查询、上下文缓存

## License

Apache License 2.0
