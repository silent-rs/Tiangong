# 消息 TPS 展示 · 设计与实施

> 分支：`feature/message-tps-display`（基于 main @ 32748b6c）
> 状态：**已实施** —— TPS 数据链路（后端真实计算 → 事件下发 → 持久化 → 前端展示）已接入主界面；
> mock 设计稿页（`tps-mock.html`）已随实现完成而移除。current token 进度条（3.4 节）设计另行进行
> （参考稿：仓库根目录 `deepseek_html_20261028_046501.html`），不在本文件范围内。

## 1. 目标

在会话界面展示两类速度指标：

1. **当前请求 TPS**：正在流式生成的这一次请求的实时输出速度（tokens/s），用户在等待回答时可见。
2. **每条消息 TPS**：历史上每条 assistant 消息的生成速度，用于对比不同模型 / 配置 / 时段的响应快慢；随消息持久化，历史回看与 token 统计直接复现。

## 2. 现状能力盘点（设计依据）

前端数据模型（`frontend/src/api/message.ts`，与后端 `tiangong-types::Message` 同构）已具备：

| 字段 | 位置 | 说明 |
| --- | --- | --- |
| `usage.completion_tokens` | assistant / notice 消息 | 该次请求输出的 token 数（已有 `usageOf`） |
| `text_elapsed_ms` | assistant 消息 | 正文生成阶段耗时 |
| `reasoning_elapsed_ms` | assistant 消息 | 思考阶段耗时 |
| `usage.model` | MessageUsage | 模型名，可用于展示 |

**结论 A**：每条消息 TPS **无需后端改动**，前端即可精确计算：
`message_tps = usage.completion_tokens / (text_elapsed_ms / 1000)`。
注意口径：`text_elapsed_ms` 只覆盖正文阶段，思考阶段的速度应单独用 reasoning 内容长度近似，或暂不展示（本设计稿正文口径）。

**结论 B**：当前请求 TPS 需要前端自行对流式 delta 计时。主界面 `useStore` 已维护 `streamingMessageId` / `streamingContent`，流式事件到达频率即数据来源，前端做**滑动窗口（约 2s）累计**即可得到实时 TPS，无需新增 IPC。

**结论 C（可选增强，非本期必须）**：若希望实时 TPS 更精确（包含 core 侧排队、首字延迟 TTFT 等口径），可由 core 在 stream 事件中附带 `first_token_at` / `tokens_since_last`；设计上保留该扩展点，但先实现纯前端方案。

## 3. UI 设计（遵循 DeepSeek Harness 设计语言）

项目内已有该设计语言的落地先例：`frontend/src/index.css` 的工具运行行扫光（`tool-run-row`，注释注明"参考 DeepSeek Harness 的 running sweep"）与 `ToolGroup.tsx` 运行中的本地跳秒——用一道缓慢扫过光带和每秒递增的秒数替代转圈与"执行中"文字，降低视觉噪音。TPS 展示沿用同一语言：**内联小字、扫光、跳秒，不用大卡片与图表**。

### 3.1 当前请求 TPS（实时，两处展示）

**主展示：消息下方扫光行。** 挂在当前流式消息正下方的一行内联扫光行（复用 `tool-run-row` 的扫光 keyframes 与样式）：
`⚡ 正在生成  87.3 tok/s · 12s`
- "正在生成"加粗小字，速度与跳秒为 `tabular-nums` 次要色；
- TPS 数字约 600ms 刷新（滑动窗口累计），跳秒与 ToolGroup 运行行同频（1s）；
- 扫光动画沿用现有 `tool-row-sweep`（2.4s ease-in-out infinite，`hsl(var(--primary)/0.12)`）；
- 轮次结束整行消失，不留占位。

**副展示：输入框上方极简常驻小字。** 输入框条左上角仅一个图标 + 数字：
`⚡ 87.3 tok/s`
- `text-xs` muted、`tabular-nums`，不占额外高度、不加底色边框，不抢输入焦点；
- 常驻占位：空闲时显示灰化 `—`（宽度稳定不跳动），流式时 600ms 刷新并点亮图标；
- 仅数字不带标签（省"速度/TOK/S"以外的任何文案），鼠标悬停或读屏（aria-label）才给完整语义。

**不做**：独立大卡片、迷你走势图、其他常驻组件（对齐 Harness"降低视觉噪音"的原则）。

### 3.2 每条消息 TPS

- 位置：assistant 消息元信息行（与 `CallUsageDetails` 触发按钮同层、同样式的安静小字 `text-[11px]` muted，hover 才出底色）。
- 形态：`87.3 tok/s`，hover 提示展开细节：
  - 首字延迟（本期 mock，接入后来自前端首 delta 计时或 core 附带）；
  - 输出 tokens（真实 `usage.completion_tokens`）、生成耗时（真实 `text_elapsed_ms`）；
  - 有真实数据时括号内展示「真实 ≈」折算值，便于与 mock 视觉对照。
- 无 `usage` / `text_elapsed_ms` 的历史消息不显示，不占位误导。

### 3.4 输入框 current token：环形进度（已废弃，另行设计）

> **本节设计已废弃**：进度条改按新参考稿（仓库根目录 `deepseek_html_20261028_046501.html`）另行设计实施，不再采用下述 DiceUI 环形方案；保留原文仅作决策过程存档（v1 沿弧渐变被否的教训仍适用：小尺寸下渐变不可读、常驻信息量优先）。进度条组件与依赖（`circular-progress` / `@base-ui/react`）随进度条实现另行提交。

现状（`MessageInput.tsx` 底部栏）：横向进度条（`h-1.5 w-20`）+ 「当前 tokens 数」+「总计 X」两个文本，hover title 有完整明细。

**v2 设计（最终）**：横向进度条与全部数值文本替换为**一个 44×44 环形进度（描边 4px）**，组件基座 DiceUI Circular Progress（shadcn 式 copy-in，底层 @base-ui/react）：

- **全部使用组件原生部件**：`CircularProgressTrack`（muted 底环）+ `CircularProgressRange`（原生 dasharray/dashoffset 计算弧长，自带 300ms 过渡与圆头端帽）+ `CircularProgressValueText`（官方居中布局）——零手写 SVG path / 渐变；
- **百分比常驻环心**：ValueText 显示 `71%`（`text-xs font-semibold tabular-nums`，颜色随档位），一眼可读，无需 hover；
- **颜色按阈值分档取纯色**（官方推荐的 currentColor 主题化路径，Range 与 ValueText 同色）：
  - `< 70%` 绿 `text-green-600 dark:text-green-500`（安全）；
  - `70–89%` 琥珀 `text-amber-600 dark:text-amber-500`（接近阈值）；
  - `≥ 90%` 红 `text-red-600 dark:text-red-500`（临界，Range 叠加 `motion-safe:animate-pulse` 缓慢脉冲提醒，`prefers-reduced-motion` 时静止）；
- 进度口径不变：`min(100, current_tokens / compression_threshold_tokens)`，顶部起点顺时针；
- hover title 保留完整明细（当前/压缩阈值/百分比/上下文上限/累计）；读屏走组件原生 `progressbar` 语义 + `label`（aria-labelledby）+ `aria-valuetext`；
- 环与 TPS 常驻小字之间用 1px 竖分隔线隔开；
- 设计稿控制条并排展示三档色演示环（45% / 78% / 96%），便于评审档位语义。

**决策记录（v1 沿弧渐变方案被否）**：v1 曾采用 24×20 环 + 6px 描边 + 双半环 SVG 渐变沿弧映射（绿→琥珀→红）+ 数值仅 hover 可见。被否原因：① 尺寸过小（内径仅 12px），三色渐变在约 56px 周长上糊成一团，档位语义不可读；② 常驻信息量为零；③ 绕开组件原生 Range 手写 path/渐变 defs 约 100 行，实现反范式。v2 以「分档纯色 + 居中百分比」用更少代码获得更强的可读性与语义。

### 3.5 会话级汇总（后续可选）

现有 `CallUsageDetails` 对话框已按调用列出 tokens；可在表格中增加「速度」列（`completion_tokens / text_elapsed_ms`），几乎零成本复用。

## 4. 实施记录（替代原 mock 策略章节）

原设计稿阶段的 mock 工具（`utils/tpsMock.ts`、`tps-mock.html` 预览页及其测试）已随真实实现完成而移除。实际数据链路与原设计的关键差异：**TPS 全部来自 stream 上的官方 usage 累计，不做任何本地 token 计算**（曾试过在 llm crate 用 BPE tokenizer 逐批计数，因逐事件全量分词 O(n²) 阻塞 SSE 读取把流拖成数秒一批，且引入额外 tokenizer 依赖，已按「统一使用 stream 累计计算」移除）。

### 4.1 后端（官方 usage 累计透传与持久化）

- **provider 事实**：Anthropic 的 `message_delta` 逐事件携带官方累计 `output_tokens`；DeepSeek Responses / OpenAI 系仅在流末（`response.completed` / 最后 chunk）给出 usage，正文期间无官方计量。
- **透传**：core 节流层（`stream_throttle`）记录流上最近一次官方 usage 的累计 `completion_tokens`，随下一批正文事件携带：`StreamEvent::{Delta,ReactText,SummaryText}.tokens`（`Option<u64>`，语义 = 该消息截至本批的**累计** output tokens；serde default + skip，旧后端 / 插件文本反馈 / DeepSeek 正文期间无此字段）。累计保存在节流缓冲，不存在「取走累计却无事件可搭」的丢失路径。
- **持久化复现**：`Message`（assistant）新增 `text_tps: Option<f64>`，`set_usage` 定稿时按 `completion_tokens / text_elapsed_ms` 算好写入（流式快照递增时重算收敛）；序列化 / 旧格式迁移 / 访问器全链路兼容，旧数据无该字段可正常加载。

### 4.2 前端（差分消费与展示）

- **实时 TPS**：`LiveTpsTracker`（`utils/tps.ts`）按「(时刻, 官方累计)」样本维护约 2s 滑动窗口，窗口内首尾差分得速度（分母下限 300ms 防尖峰；累计回退即新消息时重开窗口）；`useLiveTps` 流式期间 600ms 采样。前端零估算：字段缺失（DeepSeek 正文期间 / 旧后端 / 插件反馈）时不记样本。
- **实时展示预期**：Anthropic 全程有实时数字；**DeepSeek / OpenAI 正文期间无官方数据，扫光行只显示「正在生成 · Ns」与输入框进行态，速度数字在结束后由操作行 / 统计呈现**（真实值，非实时估算）。
- **每条消息 TPS**：`messageTpsOf` 优先读持久化 `text_tps`（历史复现），旧数据按 usage/耗时折算兼容；`MessageActions` 操作行显示 `⚡ 87.3 tok/s`（hover 明细：tokens · 耗时）。
- **token 统计**：`CallUsageDetails` 表格新增「时间」「速度」两列——时间取消息创建时间，速度读持久化 `text_tps`（无数据行显示 `—`，hover 给出 tokens/耗时明细）。

## 5. 已实施的接入路径

原「后续实施」清单已全部落地，按实际实现重述：

1. **每条消息 TPS**：`api/message.ts` 的 `messageTpsOf` / `textTpsOf`（持久化字段优先 + 折算兼容），`MessageActions` 与 `CallUsageDetails` 消费。
2. **实时 TPS**：provider 流官方 usage 累计经节流层透传（`StreamEvent.tokens` 累计语义），`LiveTpsTracker` 窗口差分 + `useLiveTps` 采样（见 4.1 / 4.2）。
3. **TTFT**：暂未实施（原设计为可选增强）。
4. **验证**：`__tests__/tps.test.ts`（前端：messageTpsOf 兼容矩阵、LiveTpsTracker 累计差分行为、formatTps）；Rust 侧 `stream_throttle` 单测（usage 累计随正文批透传、无正文不发事件、DeepSeek 无累计时字段缺失）与 `set_usage_persists_text_tps_for_replay`（持久化 / 旧数据往返）。

## 6. 范围与不做的事

- 不改模型请求协议（token 计数在后端流式消费处完成，不影响 provider 请求）。
- 不引入图表库；走势图用最简 DOM 实现。
- 思考（reasoning）阶段速度、会话级平均速度曲线：记为后续可选，不在本期（实时与持久化口径均为正文）。
- current token 进度条（原 3.4 节）移出本文件范围，另行设计（参考稿 `deepseek_html_20261028_046501.html`）。
