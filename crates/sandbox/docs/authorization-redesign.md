# 沙箱授权模型重构：Windows 默认全禁 + 按目录授权

- 日期：2026-11-23
- 分支：`feat/sandbox-path-authorization`；宿主跟进：`perf/windows-startup-app`
- 状态：实施中。Sandbox 见 #793，宿主见 #794；skill 迁移已在 #791 合并。
- 范围：Windows 沙箱授权方式、宿主策略组装与 `LaunchPolicy` 契约。macOS / Linux 不遍历目录树，宿主可统一声明相同的通用可写目录。

## 0. 决策记录（2026-11-23）

| 事项 | 结论 |
| --- | --- |
| Windows 授权原则 | 保持 AppContainer 默认全禁，只对需要的目录授权 |
| 会话工作区 | 对所有插件读写 |
| `~/.npm`、`~/.cache`、`~/.nvm`、`~/.cargo` 等通用目录 | 对所有插件读写 |
| `media/`、`tmp/` | 对所有插件读写 |
| `sessions/` | 对所有插件只读 |
| `models.json`、`server.json` 等配置 | 禁止；官方插件按权限声明暂时保留读，豁免收紧为仅官方签名插件 |
| skills | 由 skill sidecar 自己迁移：读旧位置迁入插件数据目录，完成后只从新位置读写；旧目录保留 |
| 非 Windows | 不遍历可写根：seatbelt / bwrap 按目录出规则，访问时判定 |
| 目录树校验 | 每次启动不遍历；仅在建立或重建授权记录时校验 |
| 沙箱开关 | 关闭时按需 sidecar 不经 Launcher，已留存的授权无需处理 |

## 1. 目标

1. 每次沙箱启动不再做与目录规模成正比的工作，耗时回到常量级。
2. Windows 与其他平台的可写范围一致，宿主里不再有 `#[cfg(windows)]` 的策略分支。
3. 不降低现有隔离：插件数据目录互相隔离，宿主信任件插件不可达。

## 2. 现状问题

Windows 上启动慢在启动准备门闸等待的后台流程：门闸无条件等待沙箱检查、全部插件装载、
重新验证与常驻进程串行预热，而每一次沙箱 sidecar 启动都要做与目录规模成正比的工作。
本机一次启动在 macOS 上门闸共约 3 秒，头 10 秒内 18 次沙箱启动；同一指标在历史日志里
出现过 12 秒到 63 秒的离群值。

- 授权本身已按路径做：每个根目录写一条可继承 ACE，系统向子树传播一次。
- 但 Windows 把整个存储根设为可写根（`transport/sidecar/stdio/mod.rs` 里「整个存储根并入
  可写域」的平台分支），会话、媒体、数据、日志全部在授权范围内。常驻 sidecar 的持久授权
  因策略变化反复重做；按需 sidecar 每次启动临时授予再撤销。
- Launcher 每次启动都全量遍历可写根并逐文件打开句柄，与是否发生 ACL 操作无关（见第 7 节）。
- 另有与授权无关的放大因素：每个带 sidecar 的插件启动时制品摘要算 3 次、目标程序摘要算 2 次
  （memory 插件单个 292.7MB），列在第 13 节第 4 项。

证据等级：机制由代码走查确认；耗时基线来自本机 macOS 日志、复刻遍历算法的只读测量和
GitHub `windows-2022` CI 的小目录数据。Windows 真机各阶段的实测尚未取得。App 侧已补分阶段
计时日志（沙箱检查、插件准备分阶段、sidecar 启动准备与就绪），配合
`TIANGONG_SANDBOX_DIAGNOSTICS=1` 的 Launcher 阶段输出即可在真机上量出各项占比。

## 3. 授权清单

| 对象 | 访问 | 授权给谁 | 何时建立 |
| --- | --- | --- | --- |
| 会话工作区 | 读写 | 共享身份 | 该工作区首次有会话激活时；无会话使用后撤销 |
| `~/.npm`、`~/.cache`、`~/.nvm`、`~/.cargo` 等通用目录 | 读写 | 共享身份 | 一次，清单变化时重建 |
| `media/`、`tmp/` | 读写 | 共享身份 | 同上 |
| `sessions/` | 只读 | 共享身份 | 同上 |
| `plugins/<自身>` | 只读、可执行 | 插件身份 | 插件安装或升级时 |
| `data/<自身>` | 读写 | 插件身份 | 同上 |
| `models.json`、`auth/` | 只读 | 插件身份，需官方签名且声明 `model-config.read` | 同上 |
| `server.json` | 只读 | 插件身份，需官方签名且声明 `app-storage.read` | 同上 |
| 家目录凭据（`.ssh`、`.config/gh`） | 只读 | 插件身份，仅 terminal / command | 同上 |
| 旧技能目录 `~/.tiangong/skills` | 只读 | 插件身份，仅 skill，迁移用 | 同上，长期保留 |
| 每次调用的专用临时目录 | 读写 | 当次调用 | 随调用建立和撤销，目录很小 |
| 其余（`keys/`、`sandbox/`、`app.json`、`plugins/.verifications`、其他插件数据目录等） | 禁止 | — | 不授权即不可访问 |

说明：

- 默认全禁之下，「从禁读清单移除」不再等于可读，凭据目录需要显式授权，
  所以单列在表里；是否扩大到其他插件另行决定。
- 通用目录清单由宿主策略表逐个列出，当前确定的是 `~/.npm`、`~/.cache`、`~/.nvm`、`~/.cargo`。
  解释器探测已经识别的版本管理器根目录（`.volta`、`scoop`、`.pyenv`、`.asdf` 等）可按同样方式追加。
- 插件数据目录之间不需要再写拒绝项：共享身份在 `data/<id>` 上没有授权，别的插件自然读不到。
- 配置读权限现状是「任何已签名发布者声明即开放」（`transport/connections.rs` 的
  `SensitiveStorageAccess`）。已确认收紧为仅官方签名插件。

## 4. 身份模型

现在一份授权记录绑定「程序 + 整份策略」，策略任何一项变化都要撤销再授予。改为三类记录，
存放在 `<存储根>/sandbox/grants/`：

```
common.json               共享底座：media、tmp、通用目录读写，sessions 只读
<sha256(插件 id)>.json    插件底座：程序、插件目录只读、data/<id> 读写、配置读豁免
ws-<sha256(路径)>.json    工作区：对共享身份授予读写
```

每份记录带自己的代次，由代次派生固定的能力 SID 与限制 SID。宿主启动 sidecar 时把相关的
记录路径一并交给 Launcher，Launcher 汇总各记录的身份写入进程令牌与 AppContainer 能力列表，
逐份核对权限仍在，只重建失效的那一份。

要点：

- 工作区记录授予的是**共享身份**，所以所有插件都能访问，新增工作区不影响共享底座和插件底座。
- 常驻 sidecar 是单进程服务所有会话，令牌里始终带着共享身份。工作区记录建立后，
  运行中的常驻 sidecar 立即可访问，不需要重启。
- 沙箱开关关闭时按需 sidecar 不经 Launcher，记录留着无害，重新打开后直接复用。
- 撤销工作区记录同样要对那棵树传播一次 ACL，所以放在空闲时做（没有会话再引用该工作区之后），
  不放在启动路径上；工作区目录已不存在时直接删记录。
- 固定能力 SID 只能进 AppContainer 能力列表，不能进 `CreateRestrictedToken` 的限制 SID
  列表（#655 的 os error 87）。

## 5. 契约变更

- `LaunchPolicy` / `SandboxPolicy` 新增 `readable_roots`，默认空，遵循「新增字段带默认值」。
- 新增能力名 `path_authorization`，由 Launcher 自检上报。宿主只在 Launcher 支持时按新清单组装；
  不支持时沿用现有做法，保证新旧版本可以各自升级。
- 授权记录请求从单个 `grant_cache` 扩展为列表，旧字段继续接受。
- 契约测试补充 `readable_roots` 两端一致性，以及多记录请求的解析。

## 6. 各平台处理

| 平台 | 可读根 | 可写根 | 每次启动的工作 |
| --- | --- | --- | --- |
| macOS（seatbelt） | 读默认放行、定点禁读不变，字段不产生规则 | 不变 | 编译规则文本 |
| Linux（bwrap） | 全盘只读挂载不变，字段不产生规则 | 不变 | 生成挂载参数 |
| Windows（AppContainer） | 只读 ACE，`FILE_GENERIC_READ` 加 `FILE_GENERIC_EXECUTE`，可继承 | 可写 ACE，沿用现有掩码 | 建进程；ACL 仅在记录失效时重做 |

Launcher 支持新能力时，宿主删除 `transport/sidecar/stdio/mod.rs` 中「整个存储根并入可写域」的
Windows 分支，改为对所有平台组装同一份策略；不支持时沿用现有做法。

## 7. 目录树校验（仅 Windows）

先区分两件事：

- **ACL 传播**：给目录写入可继承 ACE 时，Windows 自动把 ACE 写到子树里已有的文件上。
  这是系统行为，只在授权建立、重建、撤销时发生，不是沙箱的代码。
- **目录树校验**：Launcher 自己的安全检查（`sandbox/windows.rs` 的 `validate_writable_tree`）。
  它在每次启动、任何 ACL 操作之前执行：递归枚举每个可写根，遇到重解析点就拒绝，
  并逐文件打开句柄确认硬链接的其他名字没有超出授权目录。复用持久授权、完全不碰 ACL 时它照样跑。
  自检把 `hardlink_escape_blocked`、`symlink_escape_blocked` 列为必需项，靠的就是这道检查。

macOS 和 Linux 没有这两种遍历：seatbelt 和 bwrap 按目录出规则，访问时判定。

本设计的口径：每次启动不遍历，校验只在建立或重建授权记录时做，那一刻本来就有 ACL 传播。
完全去掉这道检查，自检的上述两项就不再成立，所以保留在授权时刻。

- 对象缩小到可写根：工作区、通用目录、`media`、`tmp`、`data/<id>`。
- 实现上改为句柄式批量枚举，从枚举结果取硬链接数，不再逐文件打开句柄。语义不变，
  具体系统调用的行为需在 Windows 上验证。
- 诊断输出补上文件数与目录数，便于和耗时对照。

## 8. 配置读权限

暂时保留，仅限官方签名插件：

| 权限 | 当前声明的插件 | 读到什么 |
| --- | --- | --- |
| `model-config.read` | memory、analyze-attachment、generate-image-openai | `models.json`，以及 `auth/` 下的 OAuth 凭据 |
| `app-storage.read` | index、mcp、prompt、scheduler、skill | `server.json` |

后续方向是改为宿主经环境变量或桥接下发端点与令牌，subagent 已是这种做法。到位后删除这两项豁免。

## 9. skills

已在 #791（skill 0.2.9）实现，由 skill sidecar 自己迁移，做法与 scheduler、mcp、subagent
在 #654 时的迁移一致，实现参照 `plugins/tiangong-plugin-scheduler/sidecar/src/migration.rs`：

1. 数据目录里没有 `.legacy-skills-migrated` 完成标记且旧目录存在时，把 `~/.tiangong/skills`
   逐项补入插件数据目录下的技能目录：不覆盖已有文件，子目录补缺失项，符号链接跳过；每个文件
   先写临时文件再原子改名，全部成功后才写完成标记。中途失败则报错并中止启动，不写标记。
2. 完成标记存在后，技能目录只指向数据目录；锁文件改在注册表根解析，不再读写旧位置。
3. 插件尚无数据目录时回退旧位置，兼容首次升级；旧目录原样保留，由用户清理。

迁移前的事实核对：

- 现状是优先读 `TIANGONG_STORAGE_ROOT/skills`，回退 `~/.tiangong/skills`
  （`plugins/tiangong-plugin-skill/sidecar/src/service.rs` 的 `resolve_skills_root`），
  `mcp-lock.json` 写在同一目录下。本机 `data/` 下没有技能数据。
- 检索 `crates/`、`src-tauri/`、`frontend/src/` 和其他插件，没有直接读取技能目录或登记文件的代码，
  只有 skill sidecar 在用。
- 存储根下的 `skills.json`、`skills-lock.json`、`mcp-lock.json` 是更早版本的遗留，现行代码不读不写，不迁移。

授权：迁移期间 skill 插件身份需要对旧目录只读。无法预知哪台机器已完成迁移，这条只读授权长期保留。

## 10. 兼容与迁移

- 发布顺序：先发 Sandbox 新契约，宿主完成支持后再上报 `path_authorization`，App 最后切新清单。
- 新 App + 旧 Launcher：能力不支持，走旧路径。
- 旧 App + 新 Launcher：新字段被忽略，行为同旧。
- 旧授权记录带旧的结构版本号，新 Launcher 首次运行时撤销后按三类记录重建。
- 保留一个回退入口，可把宿主切回「整根可写」以便排查。

## 11. 验证

- Launcher 测试：三类记录的建立、复用、重建、撤销；工作区记录对运行中进程即时生效；
  B 插件读不到 A 插件的 `data/`；未授权路径访问被拒。
- Windows CI：真实 AppContainer 测试与自检补充新字段。
- 性能：用 `TIANGONG_SANDBOX_DIAGNOSTICS=1` 取各阶段差值，用 App 侧新增的
  「桌面启动插件准备阶段耗时」取启动期总耗时，改造前后对比；另加一组大目录基准。
- 真机：全部官方插件冒烟，重点看 memory、analyze-attachment、generate-image-openai、
  mcp、terminal。

## 12. 风险

| 风险 | 对策 |
| --- | --- |
| 白名单漏项，插件在 Windows 上报拒绝访问，开发机发现不了 | 启动失败时输出被拒路径；给 macOS 加可选的严格读模式按同一清单对账 |
| 工作区、通用目录（`~/.cargo`、`~/.cache` 常有数万文件）、`tmp` 文件数大 | 授权只做一次；校验绑定记录；`tmp` 膨胀单独清理 |
| 通用目录对所有插件读写：沙箱内进程可以改写用户之后在沙箱外直接执行的程序，如 `~/.cargo/bin` 下的工具、nvm 管理的 node | 属既定取舍。若只为读包缓存可降为只读，确需安装时再按插件开放写 |
| #655 的两个故障复现 | 能力 SID 不进限制列表；锁冲突重试已在当前代码中 |
| Windows 可写范围收窄后暴露往 `data/<id>` 以外写的插件 | 真机全插件冒烟 |

## 13. 实施进度

1. #791（已合并）：skill 0.2.9 将旧技能目录迁入插件数据目录，并调整锁文件位置。
2. #793（本分支）：Sandbox 新增 `readable_roots` 与 `path_authorization` 契约；目录树校验移到
   ACL 授权建立或重建时；只读根支持目录和单个文件；Launcher 升至 0.1.17。能力暂按不支持上报。
3. #794（依赖 #793）：宿主改用统一可写清单与显式只读根，配置读豁免收紧为仅官方签名插件，
   通用目录清单独立维护，并补充启动分阶段计时日志。因 Launcher 尚未上报支持，当前行为保持不变。
4. 原 #792 的改动已拆分到 #793 与 #794，由这两个 PR 取代。

尚未完成：

- 共享底座、插件底座、工作区三类持久授权记录，以及按需 sidecar 的持久授权。
- 工作区记录的生命周期、自检只读根探针，以及能力正式上报为支持。
- 启动内制品摘要去重、门闸是否等待预热、自检探测顺序、插件自动升级时序。
- Windows 真机分阶段耗时实测，以及官方插件在收窄授权后的全量冒烟。
