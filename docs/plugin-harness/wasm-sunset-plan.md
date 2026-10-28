# WASM 桥接层淘汰与工具整合计划
> 状态：进行中（In Progress）
> 日期：2026-09-05
> 关联文档：`docs/plugin-harness/optimization-plan.md`（插件体系优化总纲）、`docs/plugin-development.md`
> 记录分支：`feature/issue-709-wasm-sunset`（memory / issue #709）
> 分支约定：每个插件的迁移在独立分支 `feature/<插件名>-wasm-sunset` 上进行，验证通过后各自合入主干
> 目标：淘汰插件 WASM 桥接层，工具声明统一进 `plugin.json`（schema v2 `tools`），工具处理统一进 sidecar，去掉「WASM 桥接 + protocol 双层转发」

---

## 1. 背景与结论

随着 schema v2 清单直连形态落地（fs、bot、terminal、browser、interaction、creator、workspace-files 均已无 wasm），
剩余插件的 WASM 组件大多是**纯桥接层**：只做工具规格声明、参数宽松解析和 IPC 转发，业务逻辑全在 sidecar。
这层桥接带来三份重复实现（wasm 工具 spec / plugin.json / protocol 操作常量）和 wasmtime 运行时开销
（`crates/plugin/tiangong-plugin-runtime` 10 个文件集成 wasmtime），应逐步淘汰。

**判别标准**（是否可按本计划迁移）：

- ✅ 纯桥接：工具 spec + 参数解析 + 转发 sidecar（fetch、generate-image-openai、analyze-attachment、command、index、mcp、memory、scheduler、skill、volcengine、screenshot-input、coding、computer-use、subagent 中的桥接部分）
- ⚠️ 非纯桥接：wasm 内有真实业务逻辑（prompt 无 sidecar，逻辑在 wasm 内），需先把逻辑下沉 sidecar 或改造为 TS 工具插件
- ✅ 终态：全部插件无 wasm 后，`tiangong-plugin-runtime` 移除 wasmtime 集成

## 2. v2 无 wasm 契约（迁移模板）

以 fs（#701）与 fetch（本分支）为参考：

1. **plugin.json（schema 2）**：
   - `tools` 数组：name / description / input_schema / timeout_ms（从原 wasm `tool_specs()` 移植）
   - `capabilities: { tools: true }`；permissions 增加 `tool.provide`（保留 `sidecar.invoke`）
   - 删除 `wasm` 字段；sidecar 保持（native sidecar 默认 lifecycle=Resident，可显式声明）
2. **sidecar**：
   - 握手 capabilities 声明 `tool:<工具名>`（与清单 `tools` 一致）
   - 工具调用 operation=工具名、payload=参数对象；返回 ToolOutcome 形状 `{ok, summary, stdout, stderr, exit_code}`
   - 工作目录经 `tiangong_plugin_sidecar::invocation_context()` 的 `RequestInvocationContext.workspace` 每次调用注入，
     **删除 set_workspace 钩子与 RwLock 状态**
   - 参数解析在 sidecar 内重写，保持原 wasm 的宽松语义（数字字段接受字符串、未知枚举回退默认）
3. **收尾**：删除 `plugins/<id>/wasm/`；根 `Cargo.toml` 移出 members 与 workspace.dependencies；
   `xtask/src/main.rs` 对应 `PluginConfig` 的 `wasm_crate`/`wasm_artifact` 置 `None`

## 3. 分阶段迁移清单

### 阶段一：试点（已完成）
| 插件 | wasm 行数 | 状态 |
| --- | --- | --- |
| fetch | 312 | ✅ 已迁移（v0.2.0，独立分支 `feature/fetch-wasm-sunset`） |

### 阶段二：简单纯桥接（schema v1，单工具或双工具）
| 插件 | wasm 行数 | 状态 |
| --- | --- | --- |
| generate-image-openai | 271 | ✅ 已迁移（v0.2.12，设置页迁 `ui.contributions` 静态贡献） |
| analyze-attachment | 246 | ✅ 已迁移 |
| command | 345 | ✅ 已迁移（146877cf） |
| skill | 425 | ✅ 已迁移（v0.2.8；动态 prompt 段落经握手 `prompt` 能力 + 固定操作 `prompt_sections`，设置页迁 app/skill.html） |
| volcengine | 425 | ✅ 已迁移（f5901bc2） |

### 阶段三：中等复杂度（v1，多工具/带状态）
| 插件 | wasm 行数 | 状态 |
| --- | --- | --- |
| index | 782 | ✅ 已迁移（#724，v0.3.x） |
| mcp | 448 | 待迁移（工具名动态，见风险） |
| scheduler | 511 | ✅ 已迁移（9c7c31d2） |
| coding | 484 | ✅ 已迁移（f15886fe） |
| screenshot-input | 151 | ✅ 已迁移（00468cc5，UI slot 保留） |

### 阶段四：大体积/多工具（v1 → v2 一起做）
| 插件 | wasm 行数 | 状态 |
| --- | --- | --- |
| memory | 1270 | ✅ 已迁移（v0.2.0，分支 `feature/issue-709-wasm-sunset`；含运行时 lifecycle→sidecar 转发、recall_memory 直连、设置页迁 `ui.contributions`） |
| subagent | 611 | ✅ 已迁移（v0.2.45；sidecar 本就按工具名分发，工具/mention/prompt 声明迁清单，turn_finished 提取迁 sidecar；附件注入经核在 core 收紧钩子后已失效，未重建） |
| computer-use | 1216 | ✅ 已迁移（v0.4.3，工具编排迁 sidecar、prompt 静态化、lifecycle 收指针、恢复窗口按钮迁 sidecar.* 桥接） |

### 阶段五：特殊形态
| 插件 | 说明 |
| --- | --- |
| prompt | ✅ 已迁移（v0.2.0；注入逻辑下沉 Rust sidecar：identity/rules/custom 三段经 `prompt: [{type: sidecar}]` 动态提供，设置页读写迁 `app/prompt.html` + `sidecar.get_prompt`/`set_prompt`，保存后经 `runtime.prompt_updated` 通知宿主重取段落；旧数据一次性迁移语义保留，存储根优先 `TIANGONG_STORAGE_ROOT`） |

### 阶段六：运行时收尾
- `crates/plugin/tiangong-plugin-runtime` 移除 wasmtime 集成（adapter/bindings/execution/loader 等 10 个文件）
- `docs/plugin-development.md` 删除 WASM 插件开发指南章节
- CI/xtask 移除 wasm 构建目标

## 4. 风险与注意事项

- **mcp 的动态工具名**：MCP 工具经转发暴露，清单静态 `tools` 无法枚举；迁移时需确认 v2 是否支持动态工具声明（`tool:*` 握手能力 + 清单占位），必要时先在 runtime 侧补能力。
- **prompt 的提示词注入**：~~不走工具调用通道，淘汰 wasm 前需确认 v2 的 prompt_sections 等价能力~~ 已由 `prompt: [{type: sidecar}]` 条目 + 固定操作 `prompt_sections` 解决（skill/prompt 先例）。
- **UI slot 插件**（screenshot-input 等）：wasm 同时贡献 UI，迁移时需将 UI 贡献改为清单声明（v2 `ui.contributions`）。
- 每个插件迁移在独立分支 `feature/<插件名>-wasm-sunset` 上进行、独立验证（check/clippy/nextest/fmt）后合入，保持可回滚；插件版本 +0.0.1 或 0.1.0 视破坏性（operation 名变化对宿主透明，属内部重构，minor 即可）。
