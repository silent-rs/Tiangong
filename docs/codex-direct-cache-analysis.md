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
3. **WebSocket 传输**（`crates/tiangong-llm/src/providers/codex/ws.rs`）：按会话
   持久连接 `wss://…/backend-api/codex/responses`，请求体加 `type: "response.create"`
   包裹并去掉 `stream` 字段；握手/发送失败按连接级降级 HTTP。会话内 ReAct 循环
   天然串行，复用同一连接，把同一会话钉在同一服务节点。
4. **出站 TLS 指纹**（同上文件，改用 `wreq`）：WS 传输从 rustls 换成 `wreq`
   （BoringSSL），TLS 参数按 Chrome 档位仿真——Chrome 与 Bun（opencode 运行时）
   同属 BoringSSL 家族，是纯 Rust 生态里最接近的现成档位。`http2(false)` +
   `http1_only()` 把 ALPN 压成 `http/1.1`（RFC 6455 Upgrade 只能走 HTTP/1.1），
   与 Bun 建立 WS 时的表现一致。身份头不改：`originator` 仍是 `tiangong`。

   指纹对照（`https://tls.peet.ws/api/all` 实测，同一机器同一出口）：

   | 客户端 | JA4 | 套件/扩展数 |
   | --- | --- | --- |
   | rustls（改造前） | `t13d1011h1_61a7ad8aa9b6_0d308c48d2a3` | 10 / 11 |
   | wreq + Chrome149 档位（改造后） | `t13d1516h1_8daaf6152771_d8a2da3f94cd` | 15 / 16 |

   改造后加密套件表即 Chrome 序列（`c02b c02c c02f c030 cca8 cca9` …），WS 升级
   经 BoringSSL 往返验证可用（`wss://ws.postman-echo.com/raw` echo 正常）。

   注：曾实现的「零产出兜底重试」按产品决策撤除——ChatGPT 直连不做静默重试。

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
- 传输路径：`grep "Codex WS 连接已建立\|Codex 请求经 WS 传输\|降级 HTTP"`——正常应为
  建连一次后持续「请求经 WS 传输」，出现「降级 HTTP」说明连接级失败已回落。
