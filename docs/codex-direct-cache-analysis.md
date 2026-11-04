# ChatGPT（Codex）直连链路的缓存命中与稳定性分析

> 结论先行：直连不稳定与缓存 0 命中不是天工请求构造的缺陷，而是 ChatGPT Codex
> 后端按「客户端多信号指纹」分桶的结果——非官方客户端形态的流量被路由到只提供
> 弱缓存、高降载的服务面。本文记录完整证据链、本仓库已落地的防御性修复，以及
> 后续对齐 opencode 形态的改造路线（已实测有效）。

## 1. 问题现象

- GPT 直连（ChatGPT 账号 OAuth）的会话出现大面积 `prompt_cache_hit_tokens = 0`
  （一个 160 请求的真实会话：100 次全量 miss，整体命中率 31%，且 59/60 次命中
  匹配的是 3~108 个请求之前的陈旧条目，紧邻命中仅 1 次）。
- 对话中随机出现「模型未产生有效回复」失败，用户感知为「第二条消息发不通」
  （实际是每请求约半数被降载 + 第一条失败的会话被丢弃重开的幸存者偏差）。
- 同一账号经 sub2api 中转则「请求即命中、几乎全中、速度快」。

## 2. 机制结论（按证据强度）

### 2.1 服务端：按客户端信号分桶（多信号叠加，无单一开关）

上游（`chatgpt.com/backend-api/codex`）至少按以下信号综合判定流量归属：

| 信号 | 正常桶（官方/体面客户端） | 降载桶 |
| --- | --- | --- |
| TLS 指纹 | 浏览器级（Bun/BoringSSL、uTLS 伪装） | 服务端 TLS 栈（rustls/OpenSSL/Go 默认） |
| originator/UA | 官方家族或知名客户端（codex_cli_rs、opencode…） | 未知自定义名 |
| 传输 | WebSocket 长连接（`/models` 新版目录每模型带 `prefer_websockets: true`） | HTTP 每请求新建连接 |
| session 路由头 | `session-id`/`session_id`（opencode 源码注释："ChatGPT routes its prompt cache on this header"） | 无 |

降载桶的行为：约半数请求收到 HTTP 200 + 流内 `server_is_overloaded` 错误事件
（或空流/空 completed）；缓存仅有跨请求内容寻址的公共块层，会话条目写入后
分钟级不可见（读取走旧副本），因此快节奏 agent loop 永远追不上。

### 2.2 模型面：由 `version` 头/`client_version` 参数门控

- 请求带真实 CLI 版本（如 `0.146.0`）时只放行该版本授权的模型子集；
- 不带版本头或高版本号（天工 `/models` 用 `client_version=99.0.0`）时放行全量
  （含 gpt-6.1-sol 等）。天工现状即靠此策略使用 sol，后续改造应保持。
- `/models` 返回按请求身份/版本变化的视图，无显式 eligibility 字段；opencode
  靠硬编码版本规则 + 黑名单维护，上游模型面轮换后即过时（实测 1.19.5 的
  5.4/5.5 系全被拒，需手动配置模型）。

### 2.3 对齐 opencode 形态的实测验证（2026-11-04，本机 + Plus 测试账号）

opencode 1.19.5（Bun TLS + WS + session-id 头 + 真名 originator）直连：
- 稳定性：连续 6 轮对话全部成功（同期天工直连失败率约 50%）；
- 缓存：紧邻全命中——`cache.read` 跟随上下文增长（10240→10368→10496，128 块
  对齐），每轮仅新增几十 token 未命中。

sub2api（官方身份伪装 + uTLS + session 头 + WS + 粘性调度）用户侧长期观察：
几乎全命中，缓存约 30 分钟闲置后过期。

## 3. 本仓库已落地的修复（feat/codex-session-id-header）

1. **session-id 会话亲和头**（`crates/tiangong-llm/src/providers/codex/`）：
   `/responses` 流式请求注入 `session-id` + `session_id` 双头，值 = 会话 id。
2. **裸 `error` 事件解析**（`crates/tiangong-llm/src/providers/openai/stream.rs`）：
   此前落入 `_ => {}` 被静默丢弃，流「正常」结束但零产出，是部分神秘空回复的
   直接原因；现转为 Error 事件走统一中断路径。
3. **零产出兜底重试**（`crates/tiangong-llm/src/provider_client.rs`）：流结束后
   正文/思考/工具调用均为空时退避 2s/5s 重试两次；耗尽后透出真实原因（流内
   错误或「上游返回空响应」），不再落入笼统的「模型未产生有效回复」。仅对
   ChatGPT 直连形态（Responses/Codex 协议）启用，DeepSeek/Anthropic 保持原语义。
   已有产出的流不重试（避免内容重复）。

预期效果：降载从「对话失败」变为「偶发延迟数秒」；错误信息可诊断。

## 4. 后续路线（按投入递增）

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0 | session-id 头 + 重试兜底 + error 事件修复 | ✅ 已完成，待功能验证 |
| P1 | 出站 TLS 指纹伪装（curl-impersonate sidecar 或 rustls 定制，Chrome/BoringSSL 级） | 未开始，最大一块 |
| P2 | WebSocket 传输（`wss://…/codex/responses`，请求体加 `type:"response.create"` 包裹，事件流同 SSE；opencode/sub2api 双实现可参照） | 未开始 |
| P3 | 模型面策略：保持动态 `/models` + 不发真实 version 头（sol 可用性依赖此） | 现状即满足 |

## 5. 验证与观测方法

- 命中日志：`grep "kv cache 命中统计" ~/.tiangong/logs/tiangong.<date>`；
- 判读口径：单请求 0% 属常态波动，看会话累计与「cached 是否随上下文增长」；
- 冷启动预期：闲置后前几分钟全 miss 属正常（条目可见性滞后 + TTL 过期）；
- 重试生效标志：日志出现「流式请求零产出，退避后重试」，对话不再中断。
