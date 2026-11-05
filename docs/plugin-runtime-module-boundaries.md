# 插件运行时职责边界

> 状态：设计稿（未实施，供拆分前评审）
> 关联：`docs/plugin-runtime-lifecycle.md`（生命周期所有权）、`docs/plugin-harness-design.md`（插件形态）、`docs/plugin-development.md`（公共中立约束）
> 适用：`crates/plugin/tiangong-plugin-runtime`

## 1. 目的与判定标准

`tiangong-plugin-runtime` 目前承载了天工插件的全部子系统：装载执行、sidecar 进程与 OS 沙箱、安装分发、签名信任、宿主桥、Core 适配、插件创作工作流。体量本身不是问题，**职责混在一个 crate、边界只靠注释维持**才是：拆模块时无法判断「移动这段代码会不会把策略带进机制层」「断开这条依赖会不会丢掉一条隐式调用」。

本文档给出拆分的安全前提：

1. **域划分**：41 个生产源文件各自归属唯一职责域（38 个归入九域，3 个是 crate 根 / 测试支撑 / 待删代码），每个域明确「拥有什么 / 不拥有什么 / 对外只有哪些入口」；
2. **依赖矩阵**：域间只允许单向依赖；当前违规逐条列出，可复核；
3. **拆分顺序**：按依赖拓扑排出 7 步，每步有前置接缝、验收标准与回滚方式。

**判定标准**：任一步拆分完成后，该域在依赖矩阵中的出边不得增加，其下游域的入边不变；度量基线（§6）中的对应数字必须下降。

## 2. 域划分

九个域 + 三项待迁出。行数为源码行数（不含测试文件）。

| 域 | 名称 | 文件数 | 行数 | 一句话职责 |
|---|---|---|---|---|
| D1 | 契约 | 3 | 2563 | 把外部输入翻译成类型并校验形状 |
| D2 | 信任 | 3 | 1819 | 回答「这份制品是否可信、由谁发布、覆盖了什么」 |
| D3 | 分发与安装 | 3 | 2756 | 把外部来源的插件变成 `plugins/` 下一条正确记录 |
| D4 | 状态机 | 7 | 3458 | 维护「装了哪些、各自什么状态」并广播变化 |
| D5 | 进程与传输 | 10 | 7283 | 把 sidecar 声明变成可用进程与连接，并把沙箱策略落到进程上 |
| D6 | 调用 | 4 | 1788 | 一次插件调用的身份、上下文、取消、进度与结果 |
| D7 | 宿主集成 | 5 | 2482 | 把插件能力接进 Core 与前端 |
| D8 | 策略 | 1 | 389 | 描述「某插件被授予什么」 |
| D9 | 创作工作流 | 2 | 681 | 把开发目录的项目变成已安装插件 |
| — | crate 根与死代码 | 3 | 666 | `lib.rs` / `test_support` / `seams`（待删） |
| — | 测试文件 | 3 | 2014 | 不参与域划分 |

合计 25899 行，与 `wc -l` 全量一致（可复核）。

### D1 契约（Contract）

- **拥有**：`plugin.json` schema 与校验规则、Slot/贡献声明类型、`LaunchPolicy` ↔ Launcher JSON 线格式、工具键（`工具名:参数=取值`）解析。
- **不拥有**：磁盘布局、进程、权限决策、展示兜底表、任何状态。
- **对外 seam**：`PluginManifest::load/parse`、`SlotRegistry`、`LaunchPolicy`（由 `tests/launch_policy_contract.rs` 与 Launcher 语义对齐）。
- **当前越界**：`manifest.rs:699-717` 调用 `tool_icons::{parse_tool_key, PLUGIN_DEFAULT_ICON_KEY}`（D7），契约反向依赖展示。
- **修法**：工具键解析下沉为 `manifest::tool_key`（或同级 `tool_key.rs`），D7 反向引用。

### D2 信任（Trust）

- **拥有**：发布者 → 公钥路由、签名清单验证、内容清单全树校验、本机用户密钥、第三方公钥登记、安装期验证记录（capabilities 快照）。
- **不拥有**：何时触发重验（编排）、安装事务、进程启动。
- **对外 seam**：`verify_signed_release(dir, &manifest)`、`sign_with_user_key`、`resolve_publisher_pubkey`、`save_verification / load_valid_capabilities / remove_verification`。
- **当前越界**：
  - `verification.rs` 的 10 处引用指向 D4：`prewarm_plugin_sidecar`、`ephemeral_sidecar_connection`、`discover_installed_plugins`、`refresh_verified_sidecar`、`set_runtime_error` 等——**记录读写与重验编排混在同一文件**；
  - `signature.rs` 3 处指向 D5：`crate::sidecar::CONTENT_MANIFEST_FILE`（该常量本身来自 `tiangong-plugin-update`）。
- **修法**：拆 `trust/record.rs`（纯记录）+ `registry/reverify.rs`（编排归 D4）；常量直接取 `tiangong_plugin_update`。

### D3 分发与安装（Distribution & Install）

- **拥有**：远程目录获取/下载/校验、staging 事务（不可变副本 + `Drop` 清理）、install/import/enable/disable/rollback/uninstall、旧 ID 迁移、安装期失败回滚。
- **不拥有**：运行中的连接与会话资源、签名算法、插件启用后的状态广播。
- **对外 seam**：`stage_plugin_archive / stage_local_plugin → StagedPlugin`、`install_staged_plugin(root, staged) → PluginStatus`、`uninstall_plugin(root, id, keep_data)`。
- **当前越界**：
  - `artifacts.rs` 混入市场展示数据（`DEFAULT_PLUGIN_IDS`、`plugin_categories`、首启标记）与 `stage_local_plugin`；
  - `migrations.rs` 5 处调 `ts_tools::cancel_plugin_calls`、3 处调 `bridge::clear_plugin_subscriptions`、1 处调 `launcher_update::revoke_persistent_grants`——**安装事务直接摆布别人的运行时资源**。
- **修法**：展示数据外移；安装/卸载收尾改为发布 `PluginChangeEvent`（D4 已具备事件总线），由订阅者停资源。

### D4 状态机（Registry / Lifecycle）

- **拥有**：`LoadedPlugin` 记录、加载/卸载/启用状态、全局操作锁、启动预加载、常驻保活、启用指纹与对话内清单、manifest 级视图查询、变化事件发布。
- **不拥有**：调用执行（D6）、连接与进程细节（D5）、Core 适配（D7）、安装事务（D3）、server 生命周期。
- **对外 seam**：`list_plugins`、`plugin_manifest(id)`、`find_installed_plugin`、`enabled_plugin_fingerprint`、`PluginChangeEvent`。
- **当前越界**：
  - `registry/mod.rs:156` 持有 `SERVER_ENDPOINT` 并在变化时重启所有 sidecar——server 生命周期状态寄生在状态机里；
  - `loader.rs` 混入 sidecar 调用、mention 来源、Core 适配器创建（见 D6/D7）；
  - `events.rs` 直接调 `bridge::emit_plugins_changed`（D7）；`views.rs` 直接构建展示表（D7）。

### D5 进程与传输（Process & Transport）

- **拥有**：进程拉起/终止/进程树、stdio 与 TCP 帧、握手、超时、取消、环境注入、**沙箱策略组装**（输入是已定策略）、连接池与换代、解释器发现、Launcher 能力探测。
- **不拥有**：插件 ID、开发目录、安装目录布局、是否启用、prompt/tool 刷新语义、插件是否可信。
- **对外 seam**：入 `SidecarConfig`（含存储布局、`HostExecutionPolicy`、运行时环境）；出 `dyn SidecarConnection`、`SidecarNotificationForwarder`、`SidecarEventHandler`。
- **当前越界**：
  - `sidecar/stdio/mod.rs` 9 处调 D4（`refresh_prompt_sections`、`refresh_tool_specs`、`dispatch_tools_recovered`、`sandbox_disabled`、`sidecars_shutting_down`、`persistent_grants_path`），2 处引用 `plugin_dev::{PLUGIN_DEV_DIR, PLUGIN_INBOX_DIR}`（D9）——**OS 沙箱组装认识插件创作目录**；
  - `sidecar.rs` 3 处调 D4 的 refresh 函数。
- **修法**：目录入 `StorageLayout`；关闭标志/沙箱开关/grants 路径入 `RuntimeEnv`；三个 refresh 改为 `SidecarEventHandler` 回调。

### D6 调用（Invocation）

- **拥有**：`RequestInvocationContext` 解析、取消顺序、TS 工具 pending 表、结果回填与进度上报、同步执行边界。
- **不拥有**：连接构造（D5）、注册表状态（D4）、前端事件通道（D7）。
- **对外 seam**：`dispatch`、`select_ts_handler`、`capabilities_handle_tool`。
- **当前越界**：`ts_plugin.rs` 4 处调 D4 的连接函数、`ts_tools.rs` 调 `registry::plugin_manifest`；`invocation.rs` 与 `ts_*` 共 12 处直接调 `bridge::*` 发事件。
- **修法**：注入 `SidecarAccess`（按 plugin_id + workspace 取连接）与 `ManifestQuery`；事件改由注入的 `RuntimeFeedback` 接口发出。

### D7 宿主集成（Host Integration）

- **拥有**：bridge 命名空间白名单与权限、原生服务路由、事件订阅表、Core 适配器聚合（`RuntimeCorePlugin`）、展示表构建。
- **不拥有**：业务实现（宿主注入）、插件状态（D4）。
- **当前越界**：`bridge.rs:233` 硬编码 `plugin-dev.` 命名空间直连 D9；同文件另外 5 个命名空间却是「宿主注入处理器」——同一张表两种接法。
- **修法**：命名空间表支持宿主注册（`register_bridge_namespace(prefix, permission, handler)`），plugin-dev 由宿主注册。

### D8 策略（Host Policy）

- **拥有**：`HostExecutionPolicy` 数据结构与默认值（全沙箱、零能力、stdio）。
- **不拥有**：「哪个插件 ID 获得什么」——这是宿主产品策略。
- **当前越界**：`resolve()` 硬编码 `terminal`/`command`/`computer-use`/`volcengine`/`plugin-creator`（`host_policy.rs:88-119`），与 `catalog_snapshot()` 的官方 ID 表。
- **修法**：`resolve` 的表外移宿主，runtime 只留 `HostPolicyProvider` 注入点，缺省 fail-closed。

### D9 创作工作流（Authoring，待迁出）

- **拥有**：开发目录项目元数据（`.plugin-dev.json`）、项目 list/status、收件箱（`.inbox`）导入、受信构建指纹登记。
- **目标**：迁出 runtime 到宿主（`src-tauri/src/plugin_authoring/`），经 D3 安装入口 + D2 签名完成，bridge 命名空间由宿主注册。
- **当前越界**：直接调 `registry::{import_staged_plugin, find_installed_plugin, plugin_install_directory}`（D3/D4）、`artifacts::stage_local_plugin`（D3）、`sidecar` 完整性校验（D5）。

### 待迁出的横切项

| 项 | 现状 | 去向 |
|---|---|---|
| `interpreter_env.rs`（1451 行） | 通用 node/python 发现埋在 D5 | 独立 crate（`tiangong-interpreter`），D5 与宿主共用 |
| `launcher_update.rs`（629 行） | 自安装/自更新 + 能力探测混在一起 | 能力探测留 D5；安装更新迁宿主基础设施 |
| `seams.rs`（151 行） | 零消费者（WASM 淘汰后遗留） | 删除 |

## 3. 域依赖矩阵

允许方向（✓ = 允许，空 = 禁止）。原则：**只能指向比自己更稳定、更少知识的域**。

| 从 \ 到 | D1 契约 | D2 信任 | D3 安装 | D4 状态机 | D5 传输 | D6 调用 | D7 宿主集成 | D8 策略 | D9 创作 |
|---|---|---|---|---|---|---|---|---|---|
| D1 契约 | — | | | | | | | | |
| D2 信任 | ✓ | — | | | | | | | |
| D3 安装 | ✓ | ✓ | — | | | | | | |
| D4 状态机 | ✓ | ✓ | ✓\* | — | ✓ | ✓ | | | |
| D5 传输 | ✓ | ✓ | | | — | | | ✓ | |
| D6 调用 | ✓ | | | | ✓ | — | | | |
| D7 宿主集成 | ✓ | | | ✓ | | ✓ | — | | |
| D8 策略 | ✓ | | | | | | | — | |
| D9 创作 | ✓ | ✓ | ✓ | | | | | | — |

\* D4→D3 仅限「安装/卸载公开入口」，状态机不实现事务细节。

### 当前违规（15 条边 / 82 处引用）

| 边 | 引用 | 位置 | 性质 | 处置 |
|---|---|---|---|---|
| D1→D7 | 4 | `manifest.rs:699-717` | 契约依赖展示键解析 | 工具键归 D1 |
| D2→D4 | 10 | `verification.rs` | 记录读写 + 重验编排混在一起 | 拆 record / reverify |
| D2→D5 | 3 | `signature.rs` | 引用传输层常量 | 常量取 update crate |
| D3→D4 | 8 | `artifacts.rs:780,793`、`migrations.rs:411,421` | 前者是安装入口 re-export（假阳性）；后者真实越界 | `assemble_*` 归 D4 内部 |
| D3→D5 | 1 | `migrations.rs` | 卸载时撤销 launcher grants | 改发事件 |
| D3→D6 | 5 | `migrations.rs` | 安装/卸载取消 TS 调用 | 改发事件 |
| D3→D7 | 3 | `migrations.rs` | 清插件订阅 | 改发事件 |
| D4→D7 | 10 | `events.rs`、`views.rs` | 事件直发 bridge；视图建展示表 | bridge 订阅事件；展示表外移 |
| D5→D4 | 9 | `sidecar.rs`、`stdio/mod.rs` | 传输层调 refresh/shutdown/grants | 注入 `RuntimeEnv` + `SidecarEvents` |
| D5→D9 | 2 | `stdio/mod.rs:604-613` | 沙箱组装认识开发目录 | 目录入 `StorageLayout` |
| D6→D4 | 6 | `ts_plugin.rs`、`ts_tools.rs` | 适配器直接取连接/清单 | 注入 `SidecarAccess` / `ManifestQuery` |
| D6→D7 | 12 | `invocation.rs`、`ts_*.rs` | 调用层直接发 bridge 事件 | 注入 `RuntimeFeedback` |
| D7→D9 | 1 | `bridge.rs:233` | 硬编码 plugin-dev 命名空间 | 命名空间可注册 |
| D9→D4 | 4 | `plugin_dev/*` | 安装入口（假阳性）+ 状态查询 | 走 D3/D4 公开入口 |
| D9→D5 | 4 | `plugin_dev/*` | 完整性校验直接调传输层 | 走 D2 的校验入口 |

## 4. 拆分前必须先立的接缝

拆 crate 之前，这七项必须先存在，否则每一步都要改动调用方。**接缝本身不改变行为**。

```rust
// 1) 存储布局（D1 级，被所有域使用）
pub struct StorageLayout { root: PathBuf }
impl StorageLayout {
    pub fn plugins(&self) -> PathBuf;              // <root>/plugins
    pub fn plugin_dir(&self, id: &str) -> PathBuf; // <root>/plugins/<id>
    pub fn dev_root(&self) -> PathBuf;             // <root>/plugins-dev
    pub fn inbox(&self) -> PathBuf;                // <root>/plugins/.inbox
    pub fn grants(&self) -> PathBuf;               // <root>/sandbox/grants
    pub fn keys(&self) -> PathBuf;                 // <root>/keys
}
// 消除：registry/artifacts/plugin_dev/stdio/bridge 各自拼路径

// 2) 宿主策略（D8）
pub type HostPolicyProvider = Arc<dyn Fn(&str, bool) -> HostExecutionPolicy + Send + Sync>;
// 缺省：全沙箱、零能力、stdio（fail-closed）；宿主启动时注入权威表

// 3) 运行时环境（D5 的输入）
pub struct RuntimeEnv {
    pub sandbox_disabled: bool,
    pub shutting_down: bool,
    pub server_endpoint: Option<(String, Option<String>)>,
    pub interpreters: InterpreterPaths,
}
// 消除：stdio → registry::{sandbox_disabled, sidecars_shutting_down, persistent_grants_path}

// 4) sidecar 事件（D5 → D4）
pub trait SidecarEvents: Send + Sync {
    fn prompt_updated(&self, plugin_id: &str);
    fn tools_updated(&self, plugin_id: &str);
    fn tools_recovered(&self, plugin_id: &str, operations: &[String]);
}
// 消除：sidecar.rs 与 stdio 对 registry refresh 函数的直接调用

// 5) 连接访问（D6 → D5）
pub trait SidecarAccess: Send + Sync {
    fn connection(&self, plugin_id: &str, workspace: Option<&Path>) -> Result<Arc<dyn SidecarConnection>>;
}
// 消除：ts_plugin 对 registry::{sidecar_connection, ephemeral_sidecar_connection...} 的 4 处调用

// 6) 宿主服务集合（D7）
pub struct HostServices {
    pub terminal: NativeServiceHandler,
    pub browser: NativeServiceHandler,
    pub webview: NativeServiceHandler,
    pub app: NativeServiceHandler,
    pub dialog: NativeServiceHandler,
    pub session_input: SessionInputHandler,
    pub emitter: BridgeEventEmitter,
    pub namespaces: Vec<RegisteredNamespace>, // 宿主可注册新命名空间（plugin-dev 走这里）
}
// 消除：12 个 set_* 全局注入点；plugin-dev 硬编码

// 7) 生命周期事件（D3 → D4/D6/D7）
// 安装/启停/卸载成功后只发布 PluginChangeEvent；停连接、取消调用、清订阅由订阅者完成
```

## 5. 拆分顺序

按依赖拓扑由叶到根。每步独立可交付、可回滚。

| 步 | 拆出 | 前置接缝 | 验收 | 风险 |
|---|---|---|---|---|
| 1 | D1 契约 → `tiangong-plugin-contract` | 工具键归位 | runtime 内 `manifest`/`slots`/`launch_policy` 只经 re-export 引用；D1 出边为 0 | 低 |
| 2 | D5 叶子（`interpreter_env`、`platform`、launcher 能力探测）→ `tiangong-plugin-process` | `StorageLayout` | 新 crate 不引用 registry/bridge/plugin_dev | 低 |
| 3 | D7 展示（`tool_icons`/`tool_titles`/`builtin_tables`）→ 宿主 | 步 1 | runtime 不再含展示兜底表；D4→D7 边消失 | 低 |
| 4 | D2 信任 → `tiangong-plugin-trust` | 拆 record / reverify | D2 无出边（除 D1）；验签 e2e 全绿 | 中 |
| 5 | D3 安装 → `tiangong-plugin-install` | `LifecycleEvents` | D3 出边只剩 D1/D2；安装/回滚/卸载 e2e 全绿 | 中 |
| 6 | D9 创作 → 宿主 | 命名空间可注册 + 步 5 | runtime 内 `rg plugin-dev` 为空；creator e2e 全绿 | 中 |
| 7 | D4/D5/D6 内核 | 全部接缝 | 跨域边 ≤ 矩阵允许集合；static 数下降 | 高 |

建议在步 1 之前先做一次**零行为改动**的整理：删 `seams.rs`、`D8` 的策略表外移、`D5→D9` 的目录常量入 `StorageLayout`。这三项不依赖任何接缝，可独立验证。

## 6. 度量基线（每步复核）

| 指标 | 当前 | 目标 |
|---|---|---|
| 跨域引用边 | 34 | ≤ 矩阵允许的 20 条 |
| 跨域引用处数 | 231 | 逐步下降 |
| 顶层模块 2-循环 | 10 组 | 0 |
| 全局 `static` | 24 | 仅保留进程级单例（如 shutdown 标志） |
| 回调注入点 | 12（`set_*` 共 14，另 2 个是普通 API） | 由 `HostServices` 一处构造替代 |
| `use super::*` | 36 处（生产代码） | 0（隐式耦合无法审计） |

复核脚本见附录 B，可在任意提交上重跑，得到同样口径的数字。

## 7. 非目标

- 不改变任何对外行为与公开 API（拆 crate 时以 re-export 保持路径稳定）。
- 不引入除 §4 七个接缝以外的新抽象；接缝未落地前不做「顺便重构」。
- 不追求把 D4/D5/D6 拆成三个 crate——它们共享 `SidecarConfig` 与调用上下文，先做模块级边界，观察一两个版本再决定。
- 不重写 sidecar 协议、沙箱策略语义与安装事务逻辑。

## 附录 A：文件归属

| 文件 | 域 | 行数 |
|---|---|---|
| `manifest.rs` | D1 契约 | 1747 |
| `slots.rs` | D1 契约 | 478 |
| `launch_policy.rs` | D1 契约 | 338 |
| `signature.rs` | D2 信任 | 765 |
| `trust.rs` | D2 信任 | 497 |
| `verification.rs` | D2 信任 | 557 |
| `artifacts.rs` | D3 安装 | 1580 |
| `registry/migrations.rs` | D3 安装 | 1076 |
| `registry/legacy.rs` | D3 安装 | 100 |
| `registry/mod.rs` | D4 状态机 | 488 |
| `registry/loader.rs` | D4 状态机 | 1442 |
| `registry/discovery.rs` | D4 状态机 | 489 |
| `registry/views.rs` | D4 状态机 | 476 |
| `registry/keepalive.rs` | D4 状态机 | 269 |
| `registry/inventory.rs` | D4 状态机 | 193 |
| `events.rs` | D4 状态机 | 101 |
| `sidecar.rs` | D5 传输 | 1882 |
| `sidecar/stdio/mod.rs` | D5 传输 | 1574 |
| `interpreter_env.rs` | D5 传输 | 1451 |
| `launcher_update.rs` | D5 传输 | 477 |
| `sidecar/stdio/process.rs` | D5 传输 | 475 |
| `sidecar/stdio/sandbox_exemptions.rs` | D5 传输 | 473 |
| `registry/connections.rs` | D5 传输 | 442 |
| `sidecar/command.rs` | D5 传输 | 336 |
| `launcher_update/windows_cache.rs` | D5 传输 | 152 |
| `platform.rs` | D5 传输 | 21 |
| `ts_plugin.rs` | D6 调用 | 985 |
| `ts_tools.rs` | D6 调用 | 428 |
| `invocation.rs` | D6 调用 | 333 |
| `execution.rs` | D6 调用 | 42 |
| `bridge.rs` | D7 宿主集成 | 911 |
| `core_bridge.rs` | D7 宿主集成 | 620 |
| `tool_icons.rs` | D7 宿主集成 | 510 |
| `tool_titles.rs` | D7 宿主集成 | 230 |
| `builtin_tables.rs` | D7 宿主集成 | 211 |
| `host_policy.rs` | D8 策略 | 389 |
| `plugin_dev/mod.rs` | D9 创作 | 486 |
| `plugin_dev/trust.rs` | D9 创作 | 195 |
| `lib.rs` | 根 | 109 |
| `test_support.rs` | 测试支撑 | 406 |
| `seams.rs` | 待删除 | 151 |
| `registry/tests.rs` | 测试 | 1135 |
| `plugin_dev/tests.rs` | 测试 | 616 |
| `sidecar/stdio/tests.rs` | 测试 | 263 |

## 附录 B：复核脚本

统计口径：按文件归属域；`crate::<模块>` 引用按模块主文件归域；跳过整行注释与行尾注释；跳过 `tests.rs` 与 `test_*.rs`；跨域才计数。

```bash
cd crates/plugin/tiangong-plugin-runtime/src && python3 - <<'PY'
import os, re, collections
PREFIX=[('registry/migrations','D3'),('registry/legacy','D3'),('registry/connections','D5'),
 ('registry','D4'),('sidecar','D5'),('plugin_dev','D9'),
 ('manifest','D1'),('slots','D1'),('launch_policy','D1'),
 ('signature','D2'),('trust','D2'),('verification','D2'),
 ('artifacts','D3'),('events','D4'),
 ('interpreter_env','D5'),('platform','D5'),('launcher_update','D5'),
 ('invocation','D6'),('execution','D6'),('ts_plugin','D6'),('ts_tools','D6'),
 ('bridge','D7'),('core_bridge','D7'),('tool_icons','D7'),('tool_titles','D7'),('builtin_tables','D7'),
 ('host_policy','D8'),('lib','D0'),('test_support','D0'),('seams','D0')]
MOD2DOM={'manifest':'D1','slots':'D1','launch_policy':'D1','signature':'D2','trust':'D2','verification':'D2',
 'artifacts':'D3','registry':'D4','events':'D4','sidecar':'D5','interpreter_env':'D5','platform':'D5',
 'launcher_update':'D5','invocation':'D6','execution':'D6','ts_plugin':'D6','ts_tools':'D6','bridge':'D7',
 'core_bridge':'D7','tool_icons':'D7','tool_titles':'D7','builtin_tables':'D7','host_policy':'D8',
 'plugin_dev':'D9','seams':'D0'}
def dom(key):
    for p,d in PREFIX:
        base=p.rstrip('/')
        if key==base or key.startswith(base+'/'): return d
edges=collections.Counter(); detail=collections.defaultdict(set)
for root,_,files in os.walk('.'):
    for f in files:
        if not f.endswith('.rs') or f=='tests.rs' or f.startswith('test_'): continue
        p=os.path.join(root,f).replace('./','')
        src=dom(p[:-3])
        if not src or src=='D0': continue
        lines=[]
        for ln in open(p).read().splitlines():
            if ln.lstrip().startswith('//'): continue
            m=re.search(r'(?<=\s)//',ln)
            lines.append(ln[:m.start()] if m else ln)
        for mod in re.findall(r'crate::([a-z_]+)',"\n".join(lines)):
            dst=MOD2DOM.get(mod)
            if dst and dst!=src and dst!='D0':
                edges[(src,dst)]+=1; detail[(src,dst)].add(p)
print("跨域边:",len(edges),"引用:",sum(edges.values()))
for (s,d),n in sorted(edges.items()):
    print(f"{s}->{d} {n:3d}  {sorted(detail[(s,d)])}")
PY
```

已知口径误差：经 re-export 调用的函数（如 `crate::registry::install_staged_plugin` 实际属 D3）会被记成 D4，统计时按附录 A 的归属人工修正。
