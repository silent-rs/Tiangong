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
3. **WebSocket 传输（唯一通道）**（`crates/tiangong-llm/src/providers/codex/ws.rs`）：
   按会话持久连接 `wss://…/backend-api/codex/responses`，请求体加
   `type: "response.create"` 包裹并去掉 `stream` 字段。会话内 ReAct 循环天然串行，
   复用同一连接，把同一会话钉在同一服务节点。
   **不再降级 HTTP**：传输形态（HTTP/WS）是不同的指纹形态，上游按形态分桶，回落
   HTTP 会打断会话亲和与前缀缓存路由；因此连接失败/首帧前断开一律在 WS 上重建
   连接重试（最多 3 次，退避 600ms×n），首帧之后的中断不重试（避免重复已产出
   内容）。无会话 id 时使用不入池的一次性连接，避免不同会话共用连接。
4. **出站 TLS 指纹**（同上文件，改用 `wreq`）：WS 传输从 rustls 换成 `wreq`
   （BoringSSL），ClientHello 逐项对齐 Bun 1.4.4（opencode 的运行时）：套件表、
   曲线顺序（含 X25519MLKEM768）、签名算法、扩展顺序、OCSP/SCT 开关全部一致，
   关闭 GREASE；ALPN 固定 `http/1.1`（RFC 6455 Upgrade 只能走 HTTP/1.1）。
   身份头不改：`originator` 仍是 `tiangong`，另补一个 `user-agent: tiangong/<ver>`
   ——真实客户端都会自报 UA，缺 UA 本身是非常规形态信号。

   指纹对照（`https://tls.peet.ws/api/all` 实测）：

   | 客户端 | JA3 | JA4 |
   | --- | --- | --- |
   | rustls（改造前） | `17566ca5ef4f00e808521ac378ef76d0` | `t13d1011h1_61a7ad8aa9b6_0d308c48d2a3` |
   | Chrome 档位（中间态，不够像） | `cbfc60b0a4d16f23a120b880c65b51ba` | `t13d1516h1_8daaf6152771_d8a2da3f94cd` |
   | **当前实现** | **`1523504b38f0fae0d881d4b6554aac1b`** | **`t13d1713h1_5b57614c22b0_7f527383e249`** |
   | Bun 1.4.4 实测（基准） | `1523504b38f0fae0d881d4b6554aac1b` | `t13d1713h1_5b57614c22b0_7f527383e249` |

   `peetprint` 亦逐字相同：`772-771|1.1|4588-29-23-24|1027-…-513|1||4865-…-53|0-10-11-13-16-18-23-35-43-45-5-51-65281`。
   可复跑的核对：`cargo test -p tiangong-llm -- --ignored codex_ws_fingerprint`。

### 3.1 「opencode 稳定、天工不稳定」的真实原因：重试，不是身份

指纹对齐后，单请求层面两侧并无质的差别（2026-11-04 同一账号、同一时间窗口）：

| 客户端 | 单请求结果 |
| --- | --- |
| opencode CLI 连续 6 次 | 6/6 成功（单次耗时仍可达 19s） |
| 我们的探针（Bun 同指纹 + opencode 全套头 + 真实体量 body） | 4/5、3/5 成功，其余为 `server_is_overloaded` / 通用 error |
| 我们的探针（Chrome 档位 + tiangong 身份） | 4/6、2/6 成功 |

opencode 的稳定来自 `ModelRetry` / `SessionRunnerRetry`：指数退避 2s→(封顶 10s)、
最多 10 次，且**只在「本轮尚未产出任何内容」时重试**（源码 `recovery:
"retry-full"` 分支与 `!recorded.outputStarted` 判定）。它把上游约 2~4 成的单请求
降载完全吃掉，外部只看到「永远成功」。天工按产品决策不做静默重试，因此每一次
降载都会直达用户。

结论：指纹改造把「几乎必降载」拉到「与 opencode 同档」，但要做到 opencode 那种
观感，仍需要零产出重试（语义上无重复内容风险，因为只在没有任何 delta 时重试）。
重试只解决「能不能用」，缓存命中由下一节的条件决定。

### 3.2 缓存命中的真正前提：前缀逐字节稳定（2026-11-04 实测）

用与生产完全相同的 WS 客户端（Bun 同指纹 + `session-id`/`session_id` 双头 +
`prompt_cache_key`，后者天工 `build_request_json` 本来就会写）跑 append-only
四轮，读 `response.usage.input_tokens_details.cached_tokens`：

| 条件 | turn1 | turn2 | turn3 | turn4 |
| --- | --- | --- | --- | --- |
| 前缀每轮都变（实验 bug：盐写在请求内） | 0% | 0% | 0% | 0% |
| 前缀跨轮稳定、`prompt_cache_key` 有 | 0% | 92% | 90% | 89% |
| 前缀跨轮稳定、`prompt_cache_key` 无 | 0% | 92% | 90% | 89% |
| 前缀稳定但被别的请求缓存过（公共块） | 93% | 96% | 95% | 94% |

三条结论：

1. **紧邻命中不需要 `prompt_cache_key`**，`session-id` 会话亲和头足够；首发请求
   天然 0%（无块可命），从第二轮起应达到 90% 上下并随上下文按 128 块递增。
2. **前缀只要有一个字符变了，其后所有块全废**——第一次实验里盐写在每轮请求内，
   四轮全部 0%，与用户观察到的现象一致。
3. 空闲后仍能命中说明公共块层跨请求内容寻址；它给出的是陈旧命中，不是紧邻命中。

因此「大面积 0 命中」的判据很明确：要么每轮请求的前缀（`instructions` 或历史
序列化）在变，要么会话亲和标识在变。为免抓包，Codex 请求会打一条零内容泄漏的
结构日志：

```
Codex 请求前缀稳定度 session_id=… comparable=true instructions_same=true
append_only=true prev_items=12 cur_items=14 first_diff_item=None
```

`instructions_same=false` → system prompt 每轮在变（含插件段落顺序/内容）；
`append_only=false` + `first_diff_item=Some(i)` → 第 i 项历史被改写或裁剪；
三者全绿仍 0 命中，则问题在会话标识或上游路由，而不是请求内容。

预期效果：降载从「对话失败」变为「偶发延迟数秒」（已由裸 error 事件解析透出真实原因）。

## 4. 后续路线（按投入递增）

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0 | session-id 头 + error 事件修复 | ✅ 已完成 |
| P1 | 出站 TLS 指纹（`wreq`/BoringSSL，Chrome 档位仿真） | ✅ 已完成（WS 传输；HTTP 回退路径仍用 rustls） |
| P2 | WebSocket 传输（`wss://…/codex/responses`，请求体加 `type:"response.create"` 包裹，事件流同 SSE） | ✅ 已完成 |
| P3 | 模型面策略：保持动态 `/models` + 不发真实 version 头（sol 可用性依赖此） | 现状即满足 |

## 5. 验证与观测方法

- 命中日志：`grep "kv cache 命中统计" ~/.tiangong/logs/tiangong.<date>`；
- 判读口径：单请求 0% 属常态波动，看会话累计与「cached 是否随上下文增长」；
- 冷启动预期：闲置后前几分钟全 miss 属正常（条目可见性滞后 + TTL 过期）；
- 传输路径：`grep "Codex WS 连接已建立\|Codex 请求经 WS 传输\|Codex WS 发送失败"`——
  正常应为建连一次后持续「请求经 WS 传输」；出现「Codex WS 发送失败」「首帧前断开」
  说明该次尝试的连接已失效，随后会在 WS 上重建重试（日志带 `attempt`），不会回落 HTTP。
  想看连接复用细节可加 `TIANGONG_LOG=info,tiangong_llm::providers::codex::ws=debug`。
