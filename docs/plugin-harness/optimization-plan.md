# 插件体系优化开发准备

> 状态：开发准备（Planning）
> 日期：2026-08-28
> 关联设计：`docs/plugin-harness-design.md`、`docs/plugin-harness/requirements.md`
> 关联文档：`docs/plugin-development.md`（WASM 插件开发指南）
> 目标：梳理当前插件体系与最新插件模式（Plugin Harness）的差距，明确优化方向与开发准备

---

## 1. 背景与目标

天工插件体系已从「原生插件 + WASM 插件」两条割裂路径演进到统一插件形态（Plugin Harness）。当前仓库 `plugins/` 下已有 22 个官方插件，其中 5 个已迁移到 schema v2 新形态，17 个仍为 schema v1 WASM 形态。

本阶段目标不是「把所有插件统一为 TS 新形态」，而是**基于每个插件的实际职责**，识别真正需要优化的点，并落地三个明确方向：

1. **工具超时统一到 Core 层**：解决 WASM 插件无工具级超时、Agent 无法自由指定超时的问题。
2. **mention 统一由 App 分组/过滤/搜索**：解决候选平铺合并、无分组、前端全量接收的问题。
3. **前端 TTS/STT 能力插件化**：让前端合成/转录复用 tts/stt 插件的 sidecar 能力，而非天工 App 自身处理。

---

## 2. 当前插件形态全景

### 2.1 schema v2（新形态，5 个）

| 插件 | 形态 | UI slot | 说明 |
| --- | --- | --- | --- |
| interaction | TS 工具插件 | session.interaction | request_user 审批/征询 |
| browser | TS 工具插件 | extension.tab | 内嵌浏览器 |
| terminal | TS 工具插件 + sidecar | extension.tab | PTY 终端 |
| plugin-creator | TS 工具插件 + node sidecar | extension.tab | 插件创作工具链 |
| screenshot-input | WASM + sidecar | session.input-action | 截图输入 |

### 2.2 schema v1（旧 WASM 形态，17 个）

| 插件 | 类型 | 工具 | 说明 |
| --- | --- | --- | --- |
| coding | 生命周期/提示词 | 4 个（project_context/preflight/checkpoint/review） | Coding 工作模式 |
| command | 工具型 | run_command/run_shell | cli/server 命令执行 |
| fs | 工具型 | 7 个文件工具 | 文件读写 |
| memory | 生命周期/提示词 | recall_memory | 记忆系统 |
| prompt | 提示词 | 无 | 注入 identity/rules/custom_prompt |
| scheduler | 工具型 | 6 个定时任务工具 | 定时调度 |
| skill | 工具型 | get_skill_detail | 技能管理 |
| index | 工具型 | index_search/search_code | 工作区索引 |
| mcp | 工具型 | 动态 mcp__*__* | MCP 工具 |
| fetch | 工具型 | web_fetch | cli/server 抓取 |
| computer-use | 工具型 | 6 个 desktop_* | 桌面控制 |
| generate-image | 工具型 | generate_image | 图片生成 |
| generate-image-openai | 工具型 | generate_image | OpenAI 图片生成 |
| generate-video | 工具型 | generate_video | 视频生成 |
| speech-to-text | 工具型 | speech_to_text | 语音识别 |
| text-to-speech | 工具型 | text_to_speech | 语音合成 |
| analyze-attachment | 工具型 | analyze_attachment | 附件分析 |

---

## 3. 优化方向一：工具超时统一到 Core 层（已取消）

> **设计决定（2026-08-29）**：本方向取消，保持既有架构——Core 层不设置工具级
> 超时，插件是否超时、超时多少由插件自行决定（TS 插件经清单 `tools[].timeout_ms`
> 限时，WASM 插件依赖 wasmtime fuel/epoch 与 sidecar `request_timeout_ms` 兜底）。
>
> 曾实现过两版并最终回退：混合版（Core 兜底 + 插件默认值 + Agent 参数覆盖，
> `timeout` 字段后改 `timeout_ms` 避免与业务参数撞名）与唯一权威版（插件侧
> 超时全部删除、Core 注入并剥离 `timeout_ms`）。回退原因：用户确认原先
> "Core 不设超时、插件自行决定"的分层即为目标设计；唯一权威版还会导致
> 已安装旧插件清单解析失效、长任务默认超时从插件声明值缩到 Core 兜底值、
> 等待类工具（desktop_wait/web_fetch）失去精细的时长控制。

---

## 4. 优化方向二：mention 统一由 App 分组/过滤/搜索

### 4.1 现状问题

当前 mention 候选链路：

```
插件侧（WASM handle_view_message / TS manifest）
  → 聚合层 collect_mention_candidates() 平铺合并（无分组、无过滤）
  → 前端 api.getMentionCandidates() 全量接收
  → 前端 filteredCandidates 本地过滤（label/value/hint 包含匹配）
  → 前端平铺渲染（无分组，只有图标区分 kind）
```

三个真实缺口：
1. **无分组**：候选被平铺合并，前端只靠图标区分 kind（skill/agent/command/其他）。候选种类多时（skill、mcp、agent、index、tts/stt 等）平铺列表混乱。
2. **无过滤**：`collect_mention_candidates()` 全量返回所有候选，前端全量接收。插件多、候选多时全量传输 + 全量渲染有性能问题。
3. **前端搜索已有但粒度粗**：前端已有 `filteredCandidates` 做包含匹配，但基于全量平铺候选，app 层未参与。

### 4.2 目标设计

**mention 统一由 App 对插件提供的候选进行分组、过滤部分显示，前端负责搜索与渲染。**

职责划分：
- **插件侧**：只负责「提供候选」（`{value, label, kind, hint}`），不实现分组/过滤。
- **App 层（collect_mention_candidates）**：按 kind 分组 + 按 kind 白名单过滤 + 每组数量上限截断，返回结构化分组数据。
- **前端**：接收分组数据，按组渲染（组标题 + 组内候选），保留现有搜索能力（组内搜索）。

### 4.3 关键设计点

| 设计点 | 决策 |
| --- | --- |
| 分组依据 | 按 `kind` 字段分组（skill/mcp/agent/index/tts/stt 等） |
| 过滤策略 | 按 kind 白名单过滤（可配置），app 不硬编码 |
| 数量上限 | 每组最多 N 个候选，防止 index 文件候选撑爆 UI |
| 前端搜索 | 基于已分组候选，按组内搜索（label/value/hint 包含匹配） |
| 数据结构 | 返回 `{ kind, label, candidates: [...] }` 分组数组 |

### 4.4 涉及改动

- `crates/plugin/tiangong-plugin-runtime/src/registry.rs`：`collect_mention_candidates` 增加分组/过滤/截断。
- `crates/tiangong-core/src/core/mod.rs`：`get_mentions` 返回分组结构。
- `frontend/src/components/MessageInput.tsx`：接收分组数据，按组渲染。
- `frontend/src/api/tauri.ts`：`getMentionCandidates` 返回类型改为分组结构。

---

## 5. 优化方向三：前端 TTS/STT 能力插件化

### 5.1 现状问题

当前前端 TTS/STT 是**天工 App 自身处理**的：

- **TTS**：前端 `api.synthesizeSpeech` → src-tauri `synthesize_speech` 命令 → `tiangong_core::media::synthesize_speech`（直接调供应商）→ 落盘 → `api.playAudioFile` → src-tauri `play_audio_file`（afplay/powershell/aplay）。
- **STT**：前端 `api.transcribeSpeech` → src-tauri `transcribe_speech` → `tiangong_core::media::transcribe_audio` → 落盘。
- **前端录音**：`useAudioRecording`（AudioContext 录麦克风 → WAV）→ `api.transcribeSpeech`。

而 **tts/stt 插件（WASM）已经存在**，sidecar 也实现了合成/转录能力，但**只作为 Agent 工具**（`text_to_speech`/`speech_to_text`），前端并没有走插件。

### 5.2 目标设计

**让前端合成/转录/播放/录音全部复用 tts/stt 插件的 sidecar 能力，而非天工 App 自身处理。**

细分归属：
- **合成**（synthesizeSpeech）：**插件化**，前端复用 tts 插件 sidecar 合成能力。
- **转录**（transcribeSpeech）：**插件化**，前端复用 stt 插件 sidecar 转录能力。
- **播放**（playAudioFile/stopAudio）：**插件化**，前端复用 tts 插件 sidecar 播放能力（替代 src-tauri 的 afplay/powershell/aplay 系统命令）。
- **录音**（useAudioRecording）：**插件化**，前端复用 stt 插件 sidecar 录音能力（替代前端 AudioContext 录音）。

> 说明：播放/录音也迁到插件，是为了让 tts/stt 插件成为完整的「语音能力提供者」，前端不再依赖天工 App 硬编码的播放/录音实现。播放由 tts 插件 sidecar 负责（跨平台系统播放），录音由 stt 插件 sidecar 负责（麦克风采集 + 编码），前端只负责 UI 编排与调用。

### 5.3 关键设计点

| 设计点 | 决策 |
| --- | --- |
| 合成归属 | 插件化，前端复用 tts 插件 sidecar |
| 转录归属 | 插件化，前端复用 stt 插件 sidecar |
| 播放归属 | 插件化，前端复用 tts 插件 sidecar（替代系统命令） |
| 录音归属 | 插件化，前端复用 stt 插件 sidecar（替代 AudioContext） |
| 前端调用通道 | 前端经插件桥接（bridge.call）调用 tts/stt 插件 sidecar，或经 Tauri 命令转发到插件 |
| 能力可用性检测 | 前端检测「tts/stt 插件是否启用」，而非「模型是否配置」 |

### 5.4 涉及改动

- `frontend/src/api/tauri.ts`：`synthesizeSpeech`/`transcribeSpeech`/`playAudioFile`/`stopAudio` 改为经插件调用。
- `frontend/src/hooks/useStreamingTts.ts`：合成、播放走插件。
- `frontend/src/hooks/useAudioRecording.ts`：录音走插件（替代 AudioContext）。
- `frontend/src/components/MessageInput.tsx`：录音、转录走插件。
- `frontend/src/components/message/MessageActions.tsx`：TTS 合成、播放走插件。
- `frontend/src/components/message/VoiceBubble.tsx`：播放走插件。
- `src-tauri/src/commands.rs`：`synthesize_speech`/`transcribe_speech`/`play_audio_file`/`stop_audio` 改为转发到插件（或前端直接经插件桥接）。
- tts/stt 插件：扩展 sidecar 能力，支持播放/录音操作。

---

## 6. 其他优化点（低优先级）

### 6.1 插件清单规范化

| 事项 | 说明 | 优先级 |
| --- | --- | --- |
| `name` 显示名 | 所有插件缺 `name` 字段，设置页/插件管理显示 ID | 中 |
| `entrypoints` 声明 | coding/memory/prompt/scheduler/skill/index/mcp/fetch 未显式声明 | 中 |
| command 与 terminal 的 run_command 描述一致性 | 两套描述不一致（command 说"命令名可含参数自动拆分"，terminal 要求 cmd+args 分离） | 中 |

### 6.2 能力型插件 mention 声明

按最新规范，mention 是给「用户可主动点名调用」的能力型插件用的。审视结果：

**适合加 mention**：generate-image、generate-video、speech-to-text、text-to-speech、scheduler、skill、mcp、computer-use（用户会主动点名）。

**不适合加 mention**：fs、command、index、coding、memory、prompt、analyze-attachment、browser、terminal、interaction、screenshot-input（Agent 内部自动调用，用户不会点名）。

> 注：本方向与「优化方向二（mention 统一由 App 分组/过滤）」配套——App 层分组后，能力型插件的 mention 会按 kind 归组展示。

---

## 7. 优先级与依赖

| 优先级 | 方向 | 依赖 |
| --- | --- | --- |
| 🔴 高 | 方向二：mention 统一由 App 分组/过滤/搜索 | 无 |
| 🔴 高 | 方向三：前端 TTS/STT 能力插件化 | 无 |
| ~~方向一：工具超时统一到 Core 层~~（已取消，保持既有架构） | — |
| 🟡 中 | 插件清单规范化（name/entrypoints/描述一致性） | 无 |
| 🟢 低 | 能力型插件 mention 声明 | 依赖方向二 |

---

## 8. 完成标准

### 方向一：工具超时统一到 Core 层（已取消）

- [x] 按设计决定回退：Core 层不设工具超时，超时由插件自行决定（见第 3 节）。

### 方向二：mention 统一由 App 分组/过滤/搜索
- [ ] `collect_mention_candidates` 按 kind 分组 + 白名单过滤 + 每组数量上限截断。
- [ ] 前端按组渲染（组标题 + 组内候选），保留组内搜索。
- [ ] 候选种类多时（skill/mcp/agent/index/tts/stt）列表清晰不混乱。
- [ ] index 插件 mention 对齐 mcp/skill 机制（handle_view_message 加 mention 分支），候选内容控制规模。

### 方向三：前端 TTS/STT 能力插件化
- [ ] 前端合成/转录复用 tts/stt 插件 sidecar 能力，而非天工 App 自身处理。
- [ ] 前端播放/录音也复用 tts/stt 插件 sidecar 能力（替代系统命令与 AudioContext）。
- [ ] 前端能力可用性检测改为「插件是否启用」。

---

## 9. 明确不做（Non-goals）

1. 不把所有 v1 WASM 插件迁移为 TS 新形态——跨入口（cli/server）纯逻辑插件用 WASM 是正确选择。
2. 不为所有插件加 mention——只给「用户可主动点名」的能力型插件加。
3. 不重写 Core 的 Agent Loop——工具超时只是统一包装，不改变执行流水线。
