# 插件生命周期 runtime 所有权

> 状态：已实施（阶段 1-3）
> 关联：issue #245（Core 构造依赖）、config-handoff（插件变化后的上下文交接）、`docs/core-architecture.md`

## 问题

插件生命周期的**执行原语**（事务、目录切换、WASM 编译、适配器管理）一直在
`tiangong-plugin-runtime`，但**编排与扇出**散落在宿主调用方：

- `notify_plugins_changed` 在 `src-tauri/commands.rs` 有 7 个成功点需要手动记得
  调用，漏一处就少一层通知（`app.rs` 的后台自动安装路径就是漏掉的实际案例）。
- Core 的插件列表是构造时快照：启停/升级/卸载由 runtime 经 Weak 就地更新
  适配器，**唯新装插件**没有任何进入既有 Core 的通道——进程运行期间安装的
  插件对所有已存在 Core 的会话不可用，直到重启或会话删除重建。
- 下载、暂存、安装、扇出的完整编排写在宿主命令里，CLI / Server / 桌面
  各入口难以共享一致的响应行为。

## 原则：意图与事实分离

```
宿主（UI）──意图──▶ runtime 生命周期状态机 ──事实(事件)──▶ 订阅者
  install(id)        安装/升级/回滚/重载/启停/卸载            ├─ 宿主 UI 刷新（Tauri emit）
  uninstall(id)      状态迁移成功即发布                       ├─ 会话上下文交接打标（handoff）
  set_enabled(..)    PluginChangeEvent                        └─ 插件页面桥（plugins.changed）
```

- **决策权在宿主**：用户意图（装哪个、信任哪个签名、进度展示）是人机交互。
- **执行权与状态机在 runtime**：迁移事务 + 事件广播全部内聚。
- **响应权在各订阅者**：宿主不再决定"变化后该通知谁"。

## 阶段 1：runtime 统一事件发布

`crates/plugin/tiangong-plugin-runtime/src/events.rs`：

- `PluginChangeEvent { kind, plugin_id, fingerprint }`——`fingerprint` 为发布时刻
  的启用插件指纹（id@version），订阅者免重算（回调内再取注册表锁有锁序风险）。
- `PluginChangeKind`：`Installed` / `Upgraded`（升级、导入替换、回滚、重载同 ID
  换代）/ `Enabled` / `Disabled` / `Uninstalled`。
- `set_plugins_changed_listener(Option<...>)`：宿主注册回调（OnceLock + Mutex，
  可覆盖、可注销）；未注册时事件静默丢弃——Server / CLI 无 UI 的入口零成本。
- 发布点收口：`install_staged_plugin_inner`（按是否已有同 ID 区分
  Installed/Upgraded）、`set_plugin_enabled`、`rollback_plugin`、
  `uninstall_plugin`、`reload_plugin`（各拆 `*_inner` 后由公开函数包装
  `announced(kind, id, result)`——成功即广播，失败原样透传）。
- 事件语义是**幂等事实快照**：无实际变化的短路成功（对已停用插件再次停用）
  也发布；订阅者按 `fingerprint` 定档自然短路（handoff 已有该机制）。
- 发布时同步下发 `bridge::emit_plugins_changed()`（插件页面沙箱桥）——主前端
  的 Tauri 事件到达不了插件沙箱。

宿主侧（`src-tauri/src/main.rs` setup）注册唯一订阅者：Tauri `plugins_changed`
emit + `mark_all_sessions_for_plugin_change(&event.fingerprint)`。
`commands.rs` 的 `notify_plugins_changed` 与 7 处调用删除；三个只为通知存在的
`AppHandle` 参数一并移除（前端 invoke 不受影响）。

## 阶段 2：Core 插件列表只持有 runtime 聚合桥

新装插件的送入缺口用**集合收敛**解决：Core 的 `plugins` 列表只持有一个
编译期成员——`RuntimeCorePlugin`（`crates/plugin/tiangong-plugin-runtime/src/core_bridge.rs`）。
对 Core 而言插件集合构造后**永不变化**（agent 执行中自持的 Core 锁不再与
任何插件集合变化交互）；已安装插件（WASM/TS 工具）的能力由桥在被调用时
向 runtime 注册表聚合：

- **工具声明**：`tool_specs()` 聚合已交付适配器的声明（prompt 置顶 + id
  字典序，与 `prepare_plugins` 的插件序一致；重名保留先注册者），聚合时
  重建工具路由表。
- **工具执行**：`handle()` 按路由表把调用转发给拥有该工具的适配器；未知
  工具名返回 `None` 交回 core 默认逻辑。
- **prompt 段落 / @提及 / exec_env / 生命周期钩子**（`set_execution_context`
  / `set_feedback_tx` / `on_config_updated` / `on_turn_*` / `on_session_*`）：
  逐适配器转发或聚合。
- **差量装载（三段式锁）**：桥内交付表的两段持锁均为纯内存微秒级操作；
  慢操作（`load_core_plugin` 的 WASM 实例化、与迁移路径的注册表锁竞争）
  全部在锁外执行——并发聚合（`tool_specs` 与 `mention_candidates` 同时
  到达）不会在桥锁上串行卡等。并发首见同一新插件时合并段只保留先插入
  者，落选适配器随 Weak 失效回收。
- **每个 Core 持独立桥实例**（交付表互不共享），与静态装配时代「各 Core
  适配器隔离 per-session 状态」语义一致。

**core / core-manager / cli / server 零改动**：`TiangongCore`、`CoreManager::
ensure_core` 签名与改造前完全一致；桌面端 `build_plugins_sync` 返回
`vec![RuntimeCorePlugin::desktop(storage_root)]`，CLI / Server 继续一次性
静态装配。

### 已知语义边界

- **`on_session_ready`**：新适配器在会话中途经视图进入，错过该一次性钩子。
  动态插件的初始化须幂等（放 `on_turn_started` / 首次调用）；`prepare_plugins`
  每轮统一调用的 `on_config_updated` / `set_execution_context` / `set_exec_env`
  对新适配器自动补齐。
- **事件时序**：`Installed` 事件在安装事务提交后、sidecar 预热完成前发布；
  订阅者若立即拉取需容忍瞬时不可用——视图拉取式天然容忍（下一轮重试）。

## 阶段 3：下载下沉 runtime

`artifacts::install_plugin_from_repository(storage_root, plugin_id, progress)`：
目录检索 → 下载暂存（progress trait 回调）→ 阻塞线程安装事务 → 计时日志。
宿主 `download_and_install_plugin` 薄化为：注入进度回调（转发 Tauri
`plugin_install_progress` 事件）+ 一行透传 + 错误字符串化。

## 各入口形态

| 入口 | Core.plugins 构成 | 事件订阅 |
|---|---|---|
| 桌面 | `[RuntimeCorePlugin]`（聚合桥，单成员） | 前端 emit + handoff 打标 |
| CLI（REPL） | 静态一次性装配 | 不注册（无 UI） |
| Server | 静态一次性装配 | 不注册（远程管理各自处理） |
| 测试 | 静态装配或注入桥 | 按需 |

## 验证锚点

- `tiangong-plugin-runtime::core_bridge`：`桥聚合_新装插件下一轮可见且卸载后消失`
  （**新装插件进入既有 Core 缺口的回归测试**）、`桥聚合_并发首见收敛`
  （并发聚合结果一致、线程不卡死——桥锁粒度回归）。
- 既有 e2e 全绿：`ui_only_plugin`、`v2_manifest_contribution`、
  `config_handoff`（压缩交接）、`model_switch`（含执行中引导消息跳过编排）。

## 附：纯 TS 工具插件 manifest 校验链（测试造插件备忘）

`schema_version: 2` + `entrypoints: ["desktop"]`（纯 TS 工具只能 desktop）+
`permissions: ["tool.provide"]` + `capabilities: { "tools": true }`。
测试进程调用 `core_plugin_ids` 前需 `tiangong_config::registry::init_from_dir`
初始化全局配置（model_requirements 过滤依赖）。

## 自制插件动态调用通道（local 签名 → 固定工具 + 对话内清单）

**核心原则：固定 tools 的 description 恒定不变，工具清单走对话历史。**
任何放进 `tools` 字段的内容变动都会打穿 KV cache 前缀；自制插件（开发场景
反复装卸）的清单因此走 append-only 的注入通道。

### 分流判据（安装时确定，终身不变）

| 签名来源 | 通道 |
|---|---|
| `tiangong-official` / 三方登记表 / 未签名 | 独立工具声明（现状） |
| `local`（用户密钥签名，创作链 `plugin_dev` 安装） | 固定通道 + 对话内清单 |

判据在 `LoadedPlugin.signed_release.publisher`（`registry::is_local_plugin`）。

### 固定工具与路由

`RuntimeCorePlugin` 声明两个 description 恒定的工具：

- `call_local_plugin`（plugin_name / function_name / args）——`handle` 同步段
  实时路由（经差量交付表，不使用聚合缓存，两次聚合之间的装卸也能正确
  路由或给出带清单的失败信息），转发到目标适配器执行；
- `list_local_plugins`——返回当前清单 JSON。

错误语义：插件不存在 / 方法不存在 / 参数缺失 → `ok:false` + 当前清单
（或可用方法列表），模型一次纠正到位——补偿后移的 schema 校验。

### 清单到达模型的路径

1. **turn 内变化**（装完即用）：插件变化事件的宿主订阅者按
   `is_local_plugin` 分流 → `ToolInjection`（`local_plugin_list`）→
   `Command::InjectTool` → 工具批次收敛后的安全点注入消息对
   （`PendingFinish` 阶段自动转 `NeedModel`，模型刚要收尾时装了新插件
   会重新考虑）。
2. **首轮基线 / 空闲期间的变化**：桥在 `on_turn_started` 比对清单内容
   hash，变化才经自留的反馈通道注入（未送达则下轮重试）。
3. **压缩折叠自愈**：注入的清单与其他 Tool 消息一样会被压缩折叠；
   最近一条清单落在 `summary_up_to` 边界之前时，即使内容未变也在下一轮
   重新注入（`inventory_injection_folded`）——模型侧「以最近一条清单
   为准」的锚点始终存在于可见历史。

清单内容与去重：`registry::local_plugin_inventory()`（纯 manifest 级，
不实例化 WASM）+ 注入通道的连续相同去重——反复聚合但清单未变不刷屏。
清单 entry 含 `functions`（工具签名）与可选 `prompt`（prompt 段落原文，
与下述 prompt 分流配套）。

### prompt 分流（与 tools 同口径）

自制插件的 `prompt` 段落**不进 system prompt**（`core_bridge` 的
`prompt_sections` 按 `is_local_plugin` 过滤）——装卸会打穿 KV cache 前缀；
内容随清单注入对话历史。官方/三方/未签名插件的 prompt 保持现状进
system prompt。纯 prompt 的自制插件同样出现在清单里（prompt 字段），
两头一致。

### 指纹与上下文交接的口径

`registry::enabled_plugin_fingerprint` 只计入**会进入模型请求前缀**的
插件：local 发布者不计入（能力经固定工具 + 清单提供）；纯 UI 插件
不计入，判据是「确定无逻辑层」的**充分条件**——manifest 无 wasm
制品、无 sidecar、无 TS tools/prompt 声明（四者皆空才排除，与发布者
无关）。注意 WASM 插件的工具声明在组件内（`tool-specs` 接缝），
manifest 的 tools/prompt 恒为空，**不能**据此判其无能力——否则官方
WASM 插件升级将漏出交接。因此自制插件的装卸/升级**不触发上下文
交接**——开发场景反复迭代不再压缩会话。

交接执行时机：插件变化点只打标（`mark_all_sessions`），压缩统一在该
会话**下一条消息的投递路径**执行（`ensure_before_deliver`）——Agent
装插件时会话必然忙，「立即压」几乎必然撞 Busy 落回投递路径，还可能与
用户消息竞态白白作废一轮压缩。交接压缩带 180s 墙钟上限（超时按 Failed
定档放行）。定档一致的重复意图（如对已启用插件再次启用）在交接入口
按指纹短路，零模型调用。压缩留痕文案由编排方注入（core 不感知「模型
切换」「插件交接」等上游概念）：模型切换前经 core-manager、插件交接经
app 层各自传入完整文案。

### 已知代价（实机观察项）

1. schema 校验后移：错误从"发不出去"变成"一次失败回合"（错误带签名补偿）；
2. 前端渲染：自制插件调用暂落 `other` 分类（可按 args.plugin_name 补）；
3. 清单消息累积：压缩折叠后由自愈重注入兜底（见上）；长开发会话观察
   重注入频率是否可接受；
4. cache 命中率：`context.rs` 的 `prompt_cache_hit_tokens` 日志按
   source 观察反复 build 场景的 hit_ratio 稳定性。
