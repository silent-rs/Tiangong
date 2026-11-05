# 插件运行时职责边界

> 状态：设计稿（未实施，供拆分前评审）
> 关联：`docs/plugin-runtime-lifecycle.md`（生命周期所有权）、`docs/plugin-harness-design.md`（插件形态）、`docs/plugin-development.md`（公共中立约束）
> 适用：`crates/plugin/tiangong-plugin-runtime`

## 0. 本轮进展（feature/plugin-runtime-architecture）

已完成（逐步验证；除第 16 步明确标注的一处授权撤销失败降级外，全部零行为改动）：

| 步 | 内容 | 效果 |
|---|---|---|
| 1 | 删除死代码 `seams.rs`（151 行 / 4 自测） | 去掉 WASM 时代遗留的空抽象 |
| 2 | 工具键解析归契约域（新 `tool_key`） | 消除 D1→D7 反向依赖 |
| 3 | 存储布局契约 `storage_layout` + `sandbox_ensured_writable` | 消除 D5→D9（沙箱组装不再认识开发目录） |
| 4 | 权威策略表归位 `host_policy/table`（不再单独出 crate） | runtime 生产代码不再含插件 ID 策略分支；桌面/CLI/Server/e2e 同源注入 |
| 5 | 一次性进程形态与官方签名要求入策略 | 去掉 `"command"` 字面量分支 |
| 6 | 传输层注入 `RuntimeEnv` + `SidecarEvents`（D5 的输入接缝） | 消除 D5→D4：沙箱开关/退出标志/授权路径与三类通知改由状态机注入 |
| 7 | 内容清单校验归信任域（新 `content_manifest`） | 消除 D2→D5 与 D9→D5（验签、创作、spawn 前复核共用一份实现） |
| 8 | 验证编排归状态机（新 `registry/reverify`） | 消除 D2→D4（信任域只留记录读写，不再起连接） |
| 9 | 桥接命名空间可注册（宿主注册 `plugin-dev.`） | 消除 D7→D9（runtime 不含创作链路由） |
| 10 | 调用层注入 `RuntimeFeedback` + `SidecarAccess`，结果抬头归调用域 | 消除 D6→D7（调用层不再直发桥接事件） |
| 11 | `SidecarAccess` 承载取连接与旁路调用 | 消除 D6→D4（调用层不再自己解析安装目录/连接形态） |
| 12 | 插件集变更广播改为注入出口；展示投影域归属修正 | 消除 D4→D7（状态机不再直连桥接） |
| 13 | 生命周期事件收尾：停用/卸载/换代后由事件发布方释放运行时资源 | 消除 D3→D6、D3→D7（安装事务不再摆布调用层与桥接层） |
| 14 | 安装编排归位：`refresh_verified_sidecar` 归 `loader`、`install_plugin_from_repository` 归安装事务 | 消除 D3→D4 的 `assemble_*` 与安装入口调用（只剩事件发布） |
| 15 | 创作域决定保留在 runtime，依赖收窄为公开入口（新增 `plugin_publisher`） | D9→D4 只剩 seam 调用（不再读 `InstalledPlugin` 内部结构） |
| 16 | Windows 卸载撤销 launcher 授权改走事件路径（`PluginChangeEvent` 携带 `storage_root`，`StorageLayout` 新增 `grants()/grant_record()`） | 消除 D3→D5（显式路径口径下最后一条违规边）；本步含一处有意的小语义变化：撤销失败降级为警告、不再中止卸载 |
| 17 | **目录按九域重组**：36 个文件移入 9 个域目录（contract/trust/distribution/registry/transport/invocation/bridge/host_policy/plugin_dev），旧公开路径全部经 lib.rs/registry 重导出保持稳定；杂居文件的 `use super::*` 全部显式化 | 目录=域，物理布局与域划分一致；度量盲区消除（见下方度量修正） |
度量（两口径，附录 B 可复核）：显式 `crate::` 路径口径下，跨域边 34 → 21、违规边 15 → 0；
第 17 步目录重组把杂居文件的 `use super::*` 显式化并改用目录判域后，**真实口径**为
跨域边 24 / 引用 192 / 违规边 3（D3→D4 三处、D3→D5 一处、D5→D4 三处，共 7 处引用）——
这些依赖在旧口径下经相对路径互相遮蔽，从未被计入；此前的「违规 0」是度量盲区假象，
本设计稿以真实口径为准。
`crate::plugin_dev` 在传输层引用清零，runtime 生产代码中的插件 ID 字面量清零
（`builtin_tables` 的展示映射仍按原计划作为过渡期例外）。附录 A/B 已补齐 `tool_key`、
`storage_layout`、`content_manifest` 三处模块的归属，并跳过 `#[cfg(test)] mod` 块；
拆分前基线用同一脚本重算，边数与违规边不变，只有引用次数口径更严。

## 1. 目的与判定标准

`tiangong-plugin-runtime` 目前承载了天工插件的全部子系统：装载执行、sidecar 进程与 OS 沙箱、安装分发、签名信任、宿主桥、Core 适配、插件创作工作流。体量本身不是问题，**职责混在一个 crate、边界只靠注释维持**才是：拆模块时无法判断「移动这段代码会不会把策略带进机制层」「断开这条依赖会不会丢掉一条隐式调用」。

本文档给出拆分的安全前提：

1. **域划分**：42 个生产源文件各自归属唯一职责域（40 个归入九域，2 个是 crate 根 / 测试支撑），每个域明确「拥有什么 / 不拥有什么 / 对外只有哪些入口」；
2. **依赖矩阵**：域间只允许单向依赖；当前违规逐条列出，可复核；
3. **拆分顺序**：按依赖拓扑排出 7 步，每步有前置接缝、验收标准与回滚方式。

**判定标准**：任一步拆分完成后，该域在依赖矩阵中的出边不得增加，其下游域的入边不变；度量基线（§6）中的对应数字必须下降。

## 2. 域划分

九个域 + 三项待迁出。行数为源码行数（不含测试文件）。

| 域 | 名称 | 文件数 | 行数 | 一句话职责 |
|---|---|---|---|---|
| D1 | 契约 | 3 | 2563 | 把外部输入翻译成类型并校验形状 |
| D2 | 信任 | 4 | 1772 | 回答「这份制品是否可信、由谁发布、覆盖了什么」 |
| D3 | 分发与安装 | 3 | 2756 | 把外部来源的插件变成 `plugins/` 下一条正确记录 |
| D4 | 状态机 | 11 | 4675 | 维护「装了哪些、各自什么状态」并广播变化 |
| D5 | 进程与传输 | 10 | 7292 | 把 sidecar 声明变成可用进程与连接，并把沙箱策略落到进程上 |
| D6 | 调用 | 5 | 1833 | 一次插件调用的身份、上下文、取消、进度与结果 |
| D7 | 宿主集成 | 2 | 1613 | 把插件能力接进 Core 与前端 |
| D8 | 策略 | 1 | 225 | 描述「某插件被授予什么」 |
| D9 | 创作工作流 | 2 | 674 | 把开发目录的项目变成已安装插件 |
| — | crate 根与测试支撑 | 2 | 518 | `lib.rs` / `test_support` |
| — | 测试文件 | 3 | 2024 | 不参与域划分 |

合计 26544 行（域目录 26039 + 根 `lib.rs` 99 + `test_support.rs` 406），与 `wc -l` 全量一致（可复核）。

### D1 契约（Contract）

- **拥有**：`plugin.json` schema 与校验规则、Slot/贡献声明类型、`LaunchPolicy` ↔ Launcher JSON 线格式、工具键（`工具名:参数=取值`）解析。
- **不拥有**：磁盘布局、进程、权限决策、展示兜底表、任何状态。
- **对外 seam**：`PluginManifest::load/parse`、`SlotRegistry`、`LaunchPolicy`（由 `tests/launch_policy_contract.rs` 与 Launcher 语义对齐）。
- **已修复**：工具键解析下沉为 `tool_key`（D1），`manifest` 与展示层都从契约取键解析；
  目录契约集中在 `storage_layout`，各域不再各自拼路径。

### D2 信任（Trust）

- **拥有**：发布者 → 公钥路由、签名清单验证、内容清单全树校验、本机用户密钥、第三方公钥登记、安装期验证记录（capabilities 快照）。
- **不拥有**：何时触发重验（编排）、安装事务、进程启动。
- **对外 seam**：`verify_signed_release(dir, &manifest)`、`content_manifest::verify`、`sign_with_user_key`、`resolve_publisher_pubkey`、`save_verification / load_valid_capabilities / remove_verification`。
- **已修复**：内容清单全树校验从传输层（`SidecarConfig::verify_integrity_manifest`）迁到
  `content_manifest`（D2），常量直接取 `tiangong-plugin-update`；验签与 sidecar 启动前复核
  共用同一实现，D2→D5 清零。
- **已修复**：`verification.rs` 只留记录读写（路径推导、制品摘要、`load/save/remove`）；
  「何时验证、复用常驻连接还是起临时连接、能力写回运行态、失败记运行错误」的编排
  迁到 `registry/reverify.rs`（D4），D2→D4 清零。

### D3 分发与安装（Distribution & Install）

- **拥有**：远程目录获取/下载/校验、staging 事务（不可变副本 + `Drop` 清理）、install/import/enable/disable/rollback/uninstall、旧 ID 迁移、安装期失败回滚。
- **不拥有**：运行中的连接与会话资源、签名算法、插件启用后的状态广播。
- **对外 seam**：`stage_plugin_archive / stage_local_plugin → StagedPlugin`、`install_staged_plugin(root, staged) → PluginStatus`、`uninstall_plugin(root, id, keep_data)`。
- **注**：市场展示数据（默认插件集合、场景分类、首启标记）**保留在本域**——它们是
  `AvailablePlugin` 这份目录视图的组成部分，宿主再包一层只会增加一份平行的数据结构；
  真正需要外移的是「插件 ID → 执行特权」这类策略（已由 D8 承载）。
- **已修复**：安装事务不再摆布别人的运行时资源。停用/卸载/换代后，挂起的工具调用由
  `events::release_plugin_resources` 取消（D4→D6，矩阵允许），桥接订阅由 `bridge` 注册的
  内部订阅者清理（D7→D4）——原先 5 处 `ts_tools::cancel_plugin_calls` 与 3 处
  `bridge::clear_plugin_subscriptions` 已从本域移除。
- **已修复**：`refresh_verified_sidecar`（重验后刷新适配器与段落，调 `assemble_*`）归 `loader`；
  `install_plugin_from_repository`（下载 + 安装编排）从 `artifacts` 移入本域事务文件，
  `artifacts` 只负责目录检索、下载与 staging。
- **已修复**：Windows 卸载前的 launcher 授权撤销改走事件路径——`PluginChangeEvent`
  携带 `storage_root`，`events::release_plugin_resources` 在 Uninstalled 时调
  `launcher_update::revoke_plugin_grants`（D4→D5，矩阵允许），本域不再引用传输层
  （D3→D5 清零，违规边归零）。

### D4 状态机（Registry / Lifecycle）

- **拥有**：`LoadedPlugin` 记录、加载/卸载/启用状态、全局操作锁、启动预加载、常驻保活、启用指纹与对话内清单、manifest 级视图查询、变化事件发布、**插件目录的展示投影**（`views` 与 `tool_icons` / `tool_titles` / `builtin_tables`——图标与标题是清单声明加内置兜底的投影，不是宿主集成）。
- **不拥有**：调用执行（D6）、连接与进程细节（D5）、Core 适配（D7）、安装事务（D3）、server 生命周期。
- **对外 seam**：`list_plugins`、`plugin_manifest(id)`、`find_installed_plugin`、
  `plugin_install_directory(id)`、`plugin_publisher(root, id)`、`enabled_plugin_fingerprint`、
  `PluginChangeEvent`。
- **已修复**：`events.rs` 的页面广播改为注入出口（`set_plugin_change_broadcaster`，桥接层在
  `install_runtime_feedback` 时注册）；`views.rs` 用到的展示投影归本域（域归属修正，非代码搬迁）。
- **当前越界**：
  - `registry/mod.rs:156` 持有 `SERVER_ENDPOINT` 并在变化时重启所有 sidecar——server 生命周期状态寄生在状态机里；
  - `loader.rs` 混入 sidecar 调用、mention 来源、Core 适配器创建（见 D6/D7）。

### D5 进程与传输（Process & Transport）

- **拥有**：进程拉起/终止/进程树、stdio 与 TCP 帧、握手、超时、取消、环境注入、**沙箱策略组装**（输入是已定策略）、连接池与换代、解释器发现、Launcher 能力探测。
- **不拥有**：插件 ID、开发目录、安装目录布局、是否启用、prompt/tool 刷新语义、插件是否可信。
- **对外 seam**：入 `SidecarConfig`（含存储布局、`HostExecutionPolicy`、`RuntimeEnv`、`SidecarEvents`）；出 `dyn SidecarConnection`、`SidecarNotificationForwarder`。
- **已修复**：开发目录经 `StorageLayout` 由连接构造层翻译成 `sandbox_ensured_writable`，
  传输层只做目录保障，不再认识 `plugin_dev`（D5→D9 清零）。
- **已修复**：传输层不再读状态机。`RuntimeEnv`（沙箱开关、退出标志、持久授权路径）与
  `SidecarEvents`（工具恢复、prompt 段落、工具规格）两个 trait 定义在 D5，由 D4 实现并随
  `SidecarConfig` 注入；`sidecar.rs` 与 `sidecar/stdio/mod.rs` 的 12 处 D4 调用清零，D5→D4 边消失。
  传输层保留「读循环只投递、阻塞 I/O 另起线程」的原语义。

### D6 调用（Invocation）

- **拥有**：`RequestInvocationContext` 解析、取消顺序、TS 工具 pending 表、结果回填与进度上报、同步执行边界。
- **不拥有**：连接构造（D5）、注册表状态（D4）、前端事件通道（D7）。
- **对外 seam**：`dispatch`、`select_ts_handler`、`capabilities_handle_tool`。
- **已修复**：事件与 App 原语改由 `RuntimeFeedback`（`runtime_feedback` 模块）出口发出，
  桥接层经 `bridge::install_runtime_feedback` 注册实现，D6→D7 清零；生命周期钩子改走注入的
  `SidecarInvoker`（`SidecarConfig` 同级的传输层 trait，装载器注入 `RegistrySidecarInvoker`）；
  `plugin_result_header` 等结果抬头归调用域（D7 反向调用，方向合法）。
- **已修复**：取连接与旁路调用统一走注入的 `SidecarAccess`（传输层 trait，装载器注入
  `RegistrySidecarAccess`）：安装目录 → 存储根 → 按需/常驻连接形态的判定留在状态机，
  调用层只拿「连接 + 是否一次性」；`extension.tab` 判定改为适配器按清单快照传入。
  D6→D4 清零。

### D7 宿主集成（Host Integration）

- **拥有**：bridge 命名空间白名单与权限、原生服务路由、事件订阅表、Core 适配器聚合（`RuntimeCorePlugin`）。
- **不拥有**：业务实现（宿主注入）、插件状态（D4）。
- **已修复**：命名空间表支持宿主注册——`register_bridge_namespace(BridgeNamespace, handler)`
  由桌面 / CLI / Server 在启动时注册 `plugin-dev.`，`bridge.rs` 不再引用 D9；权限与错误
  信息同样来自注册项，未注册时按未知 method 拒绝（fail-closed）。
- **已修复**：`events.rs` 不再直连本域，改由本域注册广播出口（D4→D7 清零）。

### D8 策略（Host Policy）

- **拥有**：`HostExecutionPolicy` 数据结构与默认值（全沙箱、零能力、stdio），以及内置
  插件 ID 的权威策略表（`host_policy/table.rs`）。
- **不拥有**：宿主运行期追加的策略覆盖——由 `set_host_policy_provider` 注入，本域只负责
  「provider 优先、缺省回落内置表」的解析顺序。
- **已修复**：权威表放在本域的 `host_policy/table.rs`——`install()` 注册默认表、
  `execution_policy()` 按插件 ID 查表，宿主只经 `host_policy::install()` 一处注入。
  该表一度外移为独立 crate `tiangong-plugin-policy`，但它只有 runtime 自己消费、没有
  第二个调用方，独立 crate 只换来一次跨 crate 组装噪音，已回融（§0 步 4）。其余模块
  仍只见 `HostExecutionPolicy` 数据结构、`set_host_policy_provider` 注入点与 fail-closed
  默认（全沙箱、零特权、stdio）。一次性进程形态与"必须官方签名"也由策略声明，
  runtime 不再出现 `"command"` 字面量分支。

### D9 创作工作流（Authoring，保留在 runtime）

- **拥有**：开发目录项目元数据（`.plugin-dev.json`）、项目 list/status、收件箱（`.inbox`）导入、
  受信构建指纹登记、`plugin-dev.*` 桥接命名空间处理器（宿主启动时注册）。
- **决定（保留）**：原计划迁出宿主，但 `plugin_dev::call` 被桌面、CLI、Server 三个入口都注册为
  桥接命名空间处理器，只迁 `src-tauri` 会让后两者失去创作链；迁成独立 crate 又只多一层依赖
  （与策略表同一结论）。因此**保留在本 crate**，改为把依赖收窄到公开 seam。
- **依赖收窄**：只剩三个公开入口——`registry::import_staged_plugin`（D3 安装入口）、
  `registry::plugin_install_directory`、`registry::plugin_publisher`（D4 公开查询，本轮新增，
  取代直接读 `InstalledPlugin.signed_release`）；完整性校验走 D2 的 `content_manifest`
  （D9→D5 清零）。
- **若将来迁出**：前置条件是创作链在三个入口共用同一份实现，届时按独立 crate 重新评估。

### 待迁出的横切项

| 项 | 现状 | 去向 |
|---|---|---|
| `interpreter_env.rs`（1451 行） | 通用 node/python 发现埋在 D5 | 独立 crate（`tiangong-interpreter`），D5 与宿主共用 |
| `launcher_update.rs`（629 行） | 自安装/自更新 + 能力探测混在一起 | 能力探测留 D5；安装更新迁宿主基础设施 |
| `seams.rs`（151 行） | 零消费者（WASM 淘汰后遗留） | 已删除 |

## 3. 域依赖矩阵

允许方向（✓ = 允许，空 = 禁止）。原则：**只能指向比自己更稳定、更少知识的域**。

| 从 \ 到 | D1 契约 | D2 信任 | D3 安装 | D4 状态机 | D5 传输 | D6 调用 | D7 宿主集成 | D8 策略 | D9 创作 |
|---|---|---|---|---|---|---|---|---|---|
| D1 契约 | — | | | | | | | | |
| D2 信任 | ✓ | — | | | | | | | |
| D3 安装 | ✓ | ✓ | — | ✓\*\* | | | | | |
| D4 状态机 | ✓ | ✓ | ✓\* | — | ✓ | ✓ | | | |
| D5 传输 | ✓ | ✓ | | | — | | | ✓ | |
| D6 调用 | ✓ | | | | ✓ | — | | | |
| D7 宿主集成 | ✓ | | | ✓ | | ✓ | — | | |
| D8 策略 | ✓ | | | | | | | — | |
| D9 创作 | ✓ | ✓ | ✓ | ✓\*\*\* | | | | | — |

\* D4→D3 仅限「安装/卸载公开入口」，状态机不实现事务细节。
\*\* D3→D4 仅限「发布生命周期事件」（`crate::events`，接缝 7）：安装事务广播事实，不读状态机状态。
\*\*\* D9→D4 仅限公开入口（`plugin_publisher` / `plugin_install_directory` / `find_installed_plugin` /
`list_plugins` / `plugin_manifest`）：创作域保留在 runtime，只经 seam 取状态。

### 当前违规（3 条边 / 7 处引用；旧口径已消的 15 条不再复现）
第 17 步目录重组把三个「杂居文件」（migrations/legacy 原在 registry/ 下属 D3、connections
原在 registry/ 下属 D5）的 `use super::*` 全部显式化后，先前被相对路径遮蔽的真实依赖浮出：

| 边 | 引用 | 位置 | 性质 | 处置方向 |
|---|---|---|---|---|
| D3→D4 | 3 | `distribution/migrations.rs`（`registry::*` glob 与 `registry::reverify`）、`distribution/legacy.rs`（`registry::*` glob） | 安装事务读注册表状态（操作锁、loaded_plugins、重载入口、目录推导） | 收窄为白名单公开入口（同 D9→D4 模式） |
| D3→D5 | 1 | `distribution/migrations.rs` → `transport::connections`（停 sidecar/断连接/清孤儿/卸载适配器） | 安装事务直接操作连接池 | 与第 13 步同法：移入 `events::release_plugin_resources` 事件路径 |
| D5→D4 | 3 | `transport/connections.rs`（`registry::*` glob、`RegistryRuntimeEnv::new`、`RegistrySidecarEvents`） | 连接池读注册表状态并注入 registry 提供的 RuntimeEnv/Events 实现 | RuntimeEnv 实现的构造移到注入点（registry 侧传入），连接池只见 trait |

注：Windows 卸载撤销授权（原 D3→D5 最后一条显式违规）已由第 16 步消除，不再复现。

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
//    已落地形态为 trait：不取快照，沙箱开关每次 spawn 重读、退出标志每次启动前重读，
//    另含 persistent_grants_path(plugin_id) -> Option<PathBuf>（非 Windows 恒 None）。
pub struct RuntimeEnv {
    pub sandbox_disabled: bool,
    pub shutting_down: bool,
    pub server_endpoint: Option<(String, Option<String>)>,
    pub interpreters: InterpreterPaths,
}
// 消除：stdio → registry::{sandbox_disabled, sidecars_shutting_down, persistent_grants_path}

// 4) sidecar 事件（D5 → D4）
//    已落地：tools_recovered(plugin_id, payload: &str)、prompt_updated、tools_updated；
//    阻塞 I/O 由传输层在读循环之外另起线程调用。
pub trait SidecarEvents: Send + Sync {
    fn prompt_updated(&self, plugin_id: &str);
    fn tools_updated(&self, plugin_id: &str);
    fn tools_recovered(&self, plugin_id: &str, operations: &[String]);
}
// 消除：sidecar.rs 与 stdio 对 registry refresh 函数的直接调用

// 5) 连接访问（D6 → D5；已落地）
pub trait SidecarAccess: Send + Sync + Debug {
    fn connection(&self, plugin_id: &str, session_workspace: Option<&Path>) -> Result<PluginConnection>; // { connection, ephemeral }
    fn invoke(&self, plugin_id: &str, operation: &str, payload: Value, workspace: Option<&Path>) -> Result<Value>;
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
| 4 | D2 信任 → `tiangong-plugin-trust` | 拆 record / reverify（已就绪） | D2 无出边（除 D1）；验签 e2e 全绿 | 中 |
| 5 | D3 安装 → `tiangong-plugin-install` | `LifecycleEvents` | D3 出边只剩 D1/D2；安装/回滚/卸载 e2e 全绿 | 中 |
| 6 | D9 创作 → 宿主 | 命名空间可注册 + 步 5 | runtime 内 `rg plugin-dev` 为空；creator e2e 全绿 | 中 |
| 7 | D4/D5/D6 内核 | 全部接缝 | 跨域边 ≤ 矩阵允许集合；static 数下降 | 高 |

建议在步 1 之前先做一次**零行为改动**的整理：删 `seams.rs`、`D8` 的策略表归位到 `host_policy/table.rs`、`D5→D9` 的目录常量入 `StorageLayout`。这三项不依赖任何接缝，可独立验证。

## 6. 度量基线（每步复核）

| 指标 | 拆分前 | 本轮后 | 目标 |
|---|---|---|---|
| 跨域引用边 | 34 | 24（旧口径 21） | ≤ 矩阵允许的 20 条 |
| 跨域引用次数 | 195 | 192（旧口径 175） | 随边数收敛 |
| 违规边 | 15 | 3（旧口径 0） | 0 |
| 违规引用次数 | 69 | 7（旧口径 0） | 0 |
| 顶层模块 2-循环 | 10 组 | 10 组 | 0 |
| 全局 `static` | 24 | 24 | 仅保留进程级单例（如 shutdown 标志） |
| 回调注入点 | 12（`set_*` 共 14，另 2 个是普通 API） | 12 | 由 `HostServices` 一处构造替代 |
| `use super::*` | 36 处（生产代码） | 36 处 | 0（隐式耦合无法审计） |
| runtime 内插件 ID 字面量 | 5 个 ID / 多处**策略分支** | 策略分支 0（策略表在 D8 的 `host_policy/table.rs`）；目录默认集合与展示映射仍留 D3/D7（`DEFAULT_PLUGIN_IDS`、分类表、`builtin_tables`） | 策略分支恒为 0；数据表按产品需要保留 |

复核脚本见附录 B，可在任意提交上重跑，得到同样口径的数字。两口径差异全部来自
`use super::*` 相对路径：旧口径只解析显式 `crate::` 前缀，目录重组（第 17 步）后
crate 内相对引用已全部显式化，目录判域口径即真实口径。

## 7. 非目标

- 不改变任何对外行为与公开 API（拆 crate 时以 re-export 保持路径稳定）。
- 不引入除 §4 七个接缝以外的新抽象；接缝未落地前不做「顺便重构」。
- 不追求把 D4/D5/D6 拆成三个 crate——它们共享 `SidecarConfig` 与调用上下文，先做模块级边界，观察一两个版本再决定。
- 不重写 sidecar 协议、沙箱策略语义与安装事务逻辑。

## 附录 A：文件归属（目录即域）
| 文件 | 域 | 行数 |
|---|---|---|
| `contract/launch_policy.rs` | D1 契约 | 338 |
| `contract/manifest.rs` | D1 契约 | 1747 |
| `contract/mod.rs` | D1 契约 | 8 |
| `contract/slots.rs` | D1 契约 | 478 |
| `contract/storage_layout.rs` | D1 契约 | 112 |
| `contract/tool_key.rs` | D1 契约 | 63 |
| `trust/content_manifest.rs` | D2 信任 | 111 |
| `trust/mod.rs` | D2 信任 | 501 |
| `trust/signature.rs` | D2 信任 | 762 |
| `trust/verification.rs` | D2 信任 | 402 |
| `distribution/artifacts.rs` | D3 安装 | 1543 |
| `distribution/legacy.rs` | D3 安装 | 104 |
| `distribution/migrations.rs` | D3 安装 | 1050 |
| `distribution/mod.rs` | D3 安装 | 6 |
| `registry/builtin_tables.rs` | D4 状态机 | 211 |
| `registry/discovery.rs` | D4 状态机 | 487 |
| `registry/events.rs` | D4 状态机 | 180 |
| `registry/inventory.rs` | D4 状态机 | 193 |
| `registry/keepalive.rs` | D4 状态机 | 269 |
| `registry/loader.rs` | D4 状态机 | 1530 |
| `registry/mod.rs` | D4 状态机 | 585 |
| `registry/reverify.rs` | D4 状态机 | 166 |
| `registry/tests.rs` | 测试 | 1136 |
| `registry/tool_icons.rs` | D4 状态机 | 487 |
| `registry/tool_titles.rs` | D4 状态机 | 230 |
| `registry/views.rs` | D4 状态机 | 476 |
| `transport/connections.rs` | D5 传输 | 465 |
| `transport/interpreter_env.rs` | D5 传输 | 1451 |
| `transport/launcher_update.rs` | D5 传输 | 495 |
| `transport/mod.rs` | D5 传输 | 9 |
| `transport/platform.rs` | D5 传输 | 21 |
| `transport/sidecar.rs` | D5 传输 | 1888 |
| `transport/sidecar/command.rs` | D5 传输 | 336 |
| `transport/sidecar/stdio/mod.rs` | D5 传输 | 1565 |
| `transport/sidecar/stdio/process.rs` | D5 传输 | 475 |
| `transport/sidecar/stdio/sandbox_exemptions.rs` | D5 传输 | 473 |
| `transport/sidecar/stdio/tests.rs` | 测试 | 263 |
| `transport/launcher_update/windows_cache.rs` | D5 传输 | 152 |
| `invocation/execution.rs` | D6 调用 | 42 |
| `invocation/mod.rs` | D6 调用 | 340 |
| `invocation/runtime_feedback.rs` | D6 调用 | 34 |
| `invocation/ts_plugin.rs` | D6 调用 | 987 |
| `invocation/ts_tools.rs` | D6 调用 | 435 |
| `bridge/core_bridge.rs` | D7 宿主集成 | 610 |
| `bridge/mod.rs` | D7 宿主集成 | 1064 |
| `host_policy/mod.rs` | D8 策略 | 228 |
| `host_policy/table.rs` | D8 策略 | 233 |
| `plugin_dev/mod.rs` | D9 创作 | 480 |
| `plugin_dev/tests.rs` | 测试 | 626 |
| `plugin_dev/trust.rs` | D9 创作 | 192 |
| `lib.rs` | 根 | 99 |
| `test_support.rs` | 测试支撑 | 406 |
合计 26544 行，与 `wc -l` 全量一致（可复核）。

## 附录 B：复核脚本（目录判域 + re-export 别名映射）

```bash
#!/bin/bash
# 附录 B（目录重组后）：src/ 目录即域，crate:: 模块名经 lib.rs re-export 别名映射到域。
cd crates/plugin/tiangong-plugin-runtime/src || exit 1
python3 - <<'PY'
import os, re, collections

# 目录前缀 → 域（物理布局与九域一致）
DIR2DOM = {'contract':'D1','trust':'D2','distribution':'D3','registry':'D4',
           'transport':'D5','invocation':'D6','bridge':'D7','host_policy':'D8',
           'plugin_dev':'D9'}

# crate::模块名 → 域。模块物理位置在域目录内；别名是 lib.rs/registry 的 re-export，
# 与物理模块同域。
MOD2DOM = {
  # D1 contract（别名经 lib.rs）
  'manifest':'D1','slots':'D1','launch_policy':'D1','tool_key':'D1','storage_layout':'D1',
  'contract':'D1',
  # D2 trust
  'signature':'D2','verification':'D2','content_manifest':'D2','trust':'D2',
  # D3 distribution
  'artifacts':'D3','distribution':'D3',
  # D4 registry（events 等移入 registry/；别名经 lib.rs）
  'registry':'D4','events':'D4','tool_icons':'D4','tool_titles':'D4','builtin_tables':'D4',
  # D5 transport（connections 移入 transport/；别名经 lib.rs）
  'sidecar':'D5','interpreter_env':'D5','launcher_update':'D5','platform':'D5',
  'transport':'D5',
  # D6 invocation（ts_plugin 等移入 invocation/；别名经 lib.rs）
  'invocation':'D6','ts_plugin':'D6','ts_tools':'D6','execution':'D6','runtime_feedback':'D6',
  # D7 bridge
  'bridge':'D7','core_bridge':'D7',
  # D8 / D9
  'host_policy':'D8','plugin_dev':'D9',
}

ALLOW={'D1':set(),'D2':{'D1'},'D3':{'D1','D2'},'D4':{'D1','D2','D3','D5','D6'},
       'D5':{'D1','D2','D8'},'D6':{'D1','D5'},'D7':{'D1','D4','D6'},'D8':{'D1'},
       'D9':{'D1','D2','D3'}}
# D9 可用的 D4 公开入口白名单（接缝豁免）。
D4_SEAM_FOR_D9={'plugin_publisher','plugin_install_directory','find_installed_plugin',
 'list_plugins','plugin_manifest','enabled_plugin_fingerprint','import_staged_plugin'}

def dom(path):
    top = path.split('/')[0]
    return DIR2DOM.get(top)

MOD_RE = re.compile(r'\s*(pub\s+)?mod\s+\w+\s*\{')
def strip_test_modules(text):
    lines = text.split('\n'); out = []; i = 0
    while i < len(lines):
        if lines[i].strip() == '#[cfg(test)]':
            j = i + 1
            while j < len(lines) and j < i + 4 and (lines[j].strip() == '' or lines[j].strip().startswith('#')):
                j += 1
            if j < len(lines) and MOD_RE.match(lines[j]):
                depth = 0; k = j
                while k < len(lines):
                    depth += lines[k].count('{') - lines[k].count('}')
                    if depth == 0: break
                    k += 1
                i = k + 1; continue
            i += 1; continue
        out.append(lines[i]); i += 1
    return '\n'.join(out)

edges = collections.Counter(); mods = collections.defaultdict(collections.Counter)
detail = collections.defaultdict(set); seam_viol = collections.Counter()
for root, _, files in os.walk('.'):
    for f in files:
        if not f.endswith('.rs') or f == 'tests.rs' or f.startswith('test_'): continue
        p = os.path.join(root, f).replace('./', '')
        src = dom(p)
        if not src: continue
        lines = []
        for ln in strip_test_modules(open(p).read()).split('\n'):
            if ln.lstrip().startswith('//'): continue
            m = re.search(r'(?<=\s)//', ln)
            lines.append(ln[:m.start()] if m else ln)
        for ln in lines:
            for mod in re.findall(r'crate::([a-z_]+)', ln):
                dst = MOD2DOM.get(mod)
                if not dst or dst == src: continue
                edges[(src, dst)] += 1; mods[(src, dst)][mod] += 1; detail[(src, dst)].add(p)
                if (src, dst) == ('D9', 'D4') and not any(fn in ln for fn in D4_SEAM_FOR_D9):
                    seam_viol[(src, dst)] += 1

print("跨域边:", len(edges), "引用:", sum(edges.values()))
for (s, d), n in sorted(edges.items()):
    print(f"{s}->{d} {n:3d}  {sorted(detail[(s,d)])}")
viol = []
for (s, d), n in edges.items():
    if d in ALLOW[s]: continue
    if (s, d) == ('D3', 'D4'): left = n - mods[(s, d)]['events']
    elif (s, d) == ('D9', 'D4'): left = seam_viol[(s, d)]
    else: left = n
    if left > 0: viol.append((s, d, left))
print("违规边:", len(viol), "引用:", sum(v[2] for v in viol))
for s, d, n in sorted(viol): print(f"  {s}->{d} {n}")
PY
```
