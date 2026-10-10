# 天工统一插件形态（Plugin Harness）设计方案

> 状态：草案（Draft）
> 日期：2026-08-16
> 关联文档：[`docs/plugin-development.md`](./plugin-development.md)（现有 WASM 插件开发指南）、[`docs/core-architecture.md`](./core-architecture.md)
> 参考：DeepSeek Harness 的「Everything is a Plugin」理念（仅借鉴其插件组织与 UI 扩展思想，**不引入 Cordis 框架**）

---

## 1. 摘要

本文提出一种新的插件形态——**天工插件 Harness（Plugin Harness）**，用于统一并取代当前「原生插件（打包进 App）」与「WASM 插件（WASM Component + 可选 sidecar）」两条割裂的路径。

新形态的核心思路是把天工的所有可扩展能力抽象为一组稳定的**能力接缝（Capability Seam）**，插件通过**贡献声明（Contribution）**把能力挂到这些接缝上。其中 UI 不再局限于设置页，而是可以真正挂载到**会话区、拓展区、侧边栏**等主界面 DOM 树中；工具、提示词、生命周期、**操作审核、用户交互（选择 / 填写）、事件订阅、存储**等能力也都统一为同一套接缝协议。

在此之上，天工现有的**嵌入式浏览器、嵌入式终端、Agent Team、操作审核、用户交互**这些「内建功能」将全部改写为基于该接缝的插件实现，从而做到「Everything is a Plugin」：三方开发者能开发的能力，官方内建功能同样能开发，二者不再有隐藏特权通道。

---

## 2. 背景与目标

### 2.1 现状与问题

当前天工存在两类插件，各有明显短板：

| 形态 | 实现方式 | 三方开发 | UI 拓展能力 | 主要问题 |
| --- | --- | --- | --- | --- |
| 原生插件 | 直接编译进 App（如 `browser`、`terminal`） | ❌ 不可行 | 通过硬编码 Tauri 命令 + 前端组件实现 | 三方无法开发；扩展 UI 必须改主应用代码 |
| WASM 插件 | WASM Component + 可选 sidecar | ⚠️ 需用 Rust + WIT | 仅设置页 iframe | 语言门槛高；UI 只能贡献设置页，无法融入会话区/拓展区 |

具体问题归纳：

1. **UI 拓展能力弱**：WASM 插件的 `plugin-ui` 接口只能声明「设置页贡献」，渲染方式是 `iframe + srcdoc` 的隔离 HTML。它无法挂载到会话区、拓展区，也无法与消息流、输入区、消息操作等主界面元素联动。

2. **原生能力硬编码**：浏览器、终端等内建能力通过 `plugin:browser|...`、`plugin:terminal|...` 这类硬编码 Tauri 命令调用，前端也有对应的 `BrowserTabContent`、`TerminalTabContent` 专用组件。每加一种「面板类」能力都要同时改后端命令和前端组件，无法复用。

3. **交互类能力原先割裂**：操作审核（审批）、Agent 需要用户选择/填写等「需要人参与」的流程曾分别由工具流水线和交互命令处理。当前方案统一为 Desktop TypeScript 插件声明的 `request_user` 工具。

4. **三方开发门槛高**：WASM 插件要求开发者掌握 Rust + `wasm32-wasip2` + WIT 绑定，且 UI 只能写裸 HTML 字符串。这与大多数「想给天工加个面板/按钮/表单」的三方开发者技能栈（JS/TS + 前端框架）不匹配。

5. **能力扩展不可控**：每新增一类扩展能力（比如「命令面板项」「状态栏项」「消息操作按钮」），都要在 WIT、Runtime、前端桥接三层同步改协议，缺乏统一的扩展点抽象。

### 2.2 目标

1. **统一插件形态**：用一个 manifest + 一套贡献协议表达所有扩展能力，原生能力与三方能力同构。
2. **UI 挂载到主界面**：插件可声明挂载到会话区、拓展区、侧边栏、状态栏等 DOM 锚点，而不仅限于设置页。
3. **内置能力插件化**：嵌入式浏览器、嵌入式终端、Agent Team、用户征询、三方 App 拓展全部改写为插件。
4. **三方开发友好**：提供 SDK、脚手架、类型定义、调试与分发工具，支持 JS/TS 编写 UI 与逻辑。
5. **可平滑演进**：新增能力 = 新增接缝，不破坏既有插件；现有 WASM 插件可平滑升级。

### 2.3 参考与设计取向

DeepSeek Harness 的核心启示是「**Everything is a Plugin**」：模型适配器、工具注册、会话日志、Agent Loop 本身都是插件，没有需要 patch 的特权核心；插件通过「贡献服务、类型化事件、可逆副作用到共享上下文」来扩展产品，卸载时注册项自动回滚。

本方案借鉴这一**组织与扩展思想**，但不引入 Cordis 框架，而是落地到天工已有的技术栈上：

- 逻辑层继续复用 **WASM Component + WIT** 作为可信、可限制、可签名的基础（Rust/任意编译到 WASM 的语言均可）。
- UI 层新增 **挂载点（Slot）+ 沙箱容器 + 宿主桥接（Host Bridge）** 三层协议，用 Web Component + Shadow DOM 实现真正的 DOM 树挂载。
- 能力抽象为 **能力接缝（Seam）**，每种 Seam 有稳定的接口契约，内置实现与三方实现走同一契约。

---

## 3. 设计原则

1. **接缝优先（Seam-first）**：任何可扩展点先定义 Seam 契约，再谈实现；新增能力从新增 Seam 开始。
2. **贡献可逆（Reversible contribution）**：插件的每次注册都是可逆副作用，卸载/禁用时自动回滚，不残留状态。
3. **最小信任（Least privilege）**：插件默认运行在受限沙箱，能力按声明的最小权限授予；特权容器（原生组件）仅对官方签名开放。
4. **框架无关（Framework-agnostic UI）**：插件 UI 通过标准 Web 技术（HTML/CSS/JS + Shadow DOM）编写，不绑定天工前端框架，也不要求插件使用特定框架。
5. **宿主中性（Neutral runtime）**：`tiangong-plugin-runtime` 保持公共、中立、业务无关，不感知具体插件 ID、工具名、操作名。
6. **渐进演进（Progressive）**：新 Seam 是增量，旧 WASM 插件路径保持兼容；内置能力分阶段迁移，不一次性重写。

---

## 4. 总体架构

### 4.1 统一插件模型

一个插件 = **一份 manifest + 一个逻辑运行时 + 零到多个 UI 贡献 + 零到多个能力贡献**。

```text
┌─────────────────────────────────────────────────────────────┐
│                        插件（Plugin）                        │
│                                                             │
│  manifest.toml / plugin.json    声明身份、权限、能力、沙箱    │
│  ├─ 逻辑运行时                   WASM Component（Rust 等）    │
│  │    ├─ 工具贡献（Tool）                                     │
│  │    ├─ 提示词贡献（Prompt）                                 │
│  │    ├─ 生命周期钩子（Lifecycle）                            │
│  ├─ Desktop TS 工具              工具逻辑 + 用户征询 UI        │
│  ├─ UI 贡献                     HTML/CSS/JS 资源 + Slot 声明   │
│  │    ├─ 会话区（session.*）                                  │
│  │    ├─ 拓展区（extension.*）                                │
│  │    ├─ 侧边栏（sidebar.*）                                  │
│  │    └─ 设置区（settings.*）                                 │
│  └─ 可选 sidecar               原生进程（数据库/原生库/长任务）│
└─────────────────────────────────────────────────────────────┘
```

宿主（天工）只负责：**加载、沙箱、资源限制、生命周期转发、sidecar 进程管理、消息桥接**。不解析插件业务负载。

### 4.2 能力接缝（Capability Seam）

能力接缝是「宿主向插件开放的一类扩展点」的契约集合。每个 Seam 定义：

- **接口（Interface）**：插件需要实现 / 可调用的一组方法或消息；
- **贡献声明（Contribution）**：插件在 manifest 里如何声明参与该 Seam；
- **内置实现（Built-in）**：天工官方提供的默认实现，本身也是一个插件；
- **路由规则（Routing）**：同类多个插件如何协同（串联 / 独占 / 可替换）。

首版接缝清单：

| Seam | 作用 | 内置实现 | 三方可扩展 |
| --- | --- | --- | --- |
| 工具（Tool） | 声明并执行 Agent 可调用的工具 | 命令、文件、搜索等工具插件 | ✅ |
| 提示词（Prompt） | 向 system prompt 注入段落 | 记忆、技能等插件 | ✅ |
| 生命周期（Lifecycle） | 会话/轮次钩子 | Memory 反刍 | ✅ |
| UI（UI） | 贡献界面到挂载点 | 浏览器/终端/设置 | ✅（本文核心） |
| Desktop TS 工具 | Desktop 端纯 TS 工具与用户征询 | `interaction` | ✅ |
| 事件（Event） | 订阅宿主事件流 | 无默认 | ✅ |
| 存储（Storage） | 数据目录与 sidecar | 无默认 | ✅ |

新增能力 = 新增一行 Seam，不影响其他 Seam 与既有插件。

### 4.3 挂载点（Slot）与宿主桥接（Host Bridge）

UI Seam 的关键是**挂载点（Slot）**：主应用 React 树中预留的稳定锚点。插件声明把 UI 贡献挂到某个 Slot，宿主在该位置创建标准容器渲染插件 UI。

无论哪个 Slot、哪种沙箱容器，插件 UI 与宿主的通信都统一走**宿主桥接（Host Bridge）**，保证协议一致：

```text
插件 UI（沙箱内）
   │  bridge.call(method, payload) / bridge.on(channel, cb) / bridge.context
   ▼
宿主桥接层（Host Bridge）
   │  鉴权 + 白名单校验 + 序列化 + 事件路由
   ▼
逻辑运行时（WASM） / Desktop TS 工具 / 内置服务
```

### 4.4 整体数据流

```text
┌──────────────┐  manifest 贡献声明  ┌──────────────────────┐
│  插件运行时   │ ─────────────────▶ │  能力注册表（Seam Hub）│
│  (WASM+UI)   │                     │  Tool/Prompt/UI/...   │
└──────┬───────┘                     └──────────┬───────────┘
       │ bridge.call                            │ 路由
       ▼                                        ▼
┌──────────────┐                        ┌──────────────────┐
│ 宿主桥接层    │ ◀──── 调用 / 事件 ────▶│  天工 Core / 服务  │
└──────┬───────┘                        └──────────────────┘
       │ 渲染
       ▼
┌──────────────────────────────────────────────────────┐
│  主界面 DOM 树（会话区 / 拓展区 / 侧边栏 / 设置）       │
│  <tg-slot name="extension.tab">  ← Shadow/iframe 容器  │
└──────────────────────────────────────────────────────┘
```

---

## 5. 插件清单（Manifest）扩展

现有 `plugin.json` 保留并向后兼容，新增能力声明字段。示意（`schema_version` 升到 `2`）：

```jsonc
{
  "schema_version": 2,
  "id": "com.example.board",
  "name": "看板",
  "version": "1.0.0",
  "wasm": { "binary": "board.wasm" },
  "sidecar": { "binary": "board-sidecar" },          // 可选，同现状

  // 能力声明（新增）
  "capabilities": {
    "tools": true,            // 实现工具接缝（WASM 内 tool-specs/handle-tool）
    "prompt": true,           // 实现提示词接缝
    "lifecycle": true,        // 实现生命周期接缝
    "interaction": false,     // 是否贡献用户交互界面
    "events": ["session.*", "tool.*"]  // 订阅的事件命名空间
  },

  // UI 贡献（新增，核心）
  "ui": {
    "sandbox": "shadow",      // shadow | iframe | native
    "contributions": [
      {
        "slot": "extension.tab",        // 挂载点
        "id": "board-tab",
        "title": "看板",
        "icon": "board",                 // 图标名或内联 SVG
        "entry": "index.html",           // 入口 HTML（相对插件目录）
        "open_mode": "multi",            // singleton（单例 tab）| multi（多 tab）
        "context": ["session", "workspace"]  // 需要注入的上下文
      },
      {
        "slot": "session.message-item",
        "id": "board-message-action",
        "entry": "action.html",
        "singleton": false,
        "context": ["session", "message"]
      }
    ]
  },

  // 权限（扩展现有 permissions）
  "permissions": [
    "ui.shadow",            // 允许 Shadow DOM 挂载
    "bridge.call",          // 允许调用宿主桥接
    "bridge.events",        // 允许订阅事件
    "storage.private"       // 私有数据目录
  ]
}
```

关键约束（沿用并扩展现状）：

- `schema_version` 为 `2` 时校验 `capabilities` / `ui`；为 `1` 时按现有规则解析，`ui` 仅支持 `settings.*`。
- `ui.sandbox = native` 仅对携带有效官方签名的插件开放（等价现有 sidecar 签名约束）。
- `ui.contributions[].slot` 必须是宿主登记的合法 Slot ID，未知 Slot 在导入时拒绝并提示版本不匹配。
- `ui.contributions[].open_mode` 仅对 `extension.tab` 生效：`singleton` 表示该 App 全局至多一个 tab（重复打开聚焦已有实例），`multi` 表示每次打开新建 tab；缺省为 `singleton`。
- `permissions` 中敏感项（`storage.app`、`native`）需显式授权。

---

（下接第 6 章 UI 拓展体系）
---

## 6. UI 拓展体系（核心）

### 6.1 挂载点（Slot）目录

Slot 用点分层级的稳定字符串 ID 标识。宿主登记一份 Slot 目录（Slot Registry），每个 Slot 声明：挂载位置、单/多实例、可注入的上下文、默认沙箱级别、是否仅官方。

首版 Slot 目录（可扩展，见第 12 章）：

**会话区（session）**

| Slot | 位置 | 实例 | 上下文 | 说明 |
| --- | --- | --- | --- | --- |
| `session.turn-node` | 消息流中，作为独立节点插入 | 多 | session, turn | 在对话流中插入自定义卡片/进度条/结果块 |
| `session.message-item` | 每条消息的附加区；`render: "replace"` 时按消息渲染声明接管显示 | 多（按消息绑定） | session, message | 消息下方的自定义内容，如附件渲染、结构化卡片；或替换整条消息的显示（6.5.1） |
| `session.message-action` | 消息操作按钮区 | 多 | session, message | 在「复制/重试」旁新增动作按钮 |
| `session.before-input` | 输入框上方 | 多 | session | 输入上下文提示、快捷操作条 |
| `session.after-input` | 输入框下方 | 多 | session | 附加输入辅助区 |
| `session.interaction` | 覆盖输入区并向上展开的临时交互层 | 单 | session | 审批、确认、选择和输入请求；普通内容完整显示，超长内容仅滚动中间区域；显示时锁定输入区，不进入消息历史 |
| `session.empty-state` | 空会话占位 | 多 | session, workspace | 自定义新会话引导 |

**拓展区（extension，即现有 workspace panel）**

| Slot | 位置 | 实例 | 上下文 | 说明 |
| --- | --- | --- | --- | --- |
| `extension.tab` | 拓展区新增标签页（App） | 多 | session, workspace | 浏览器/终端同级的自定义面板；按 `open_mode` 决定单例/多 tab |
| `extension.side` | 拓展区侧栏 | 多 | session | 拓展区内部的辅助侧栏 |

**侧边栏（sidebar）**

| Slot | 位置 | 实例 | 上下文 | 说明 |
| --- | --- | --- | --- | --- |
| `sidebar.nav-item` | 导航项 | 多 | workspace | 新增导航入口 |
| `sidebar.panel` | 侧边栏面板区 | 单 | workspace | 全高度侧栏面板 |
| `sidebar.bottom` | 侧边栏底部 | 多 | workspace | 状态/快捷入口 |

**设置区（settings，兼容现状）**

| Slot | 位置 | 实例 | 上下文 | 说明 |
| --- | --- | --- | --- | --- |
| `settings.plugin-page` | 设置中的插件页 | 多 | 无 | 等价现有 `contributions`，平滑迁移 |

**全局（global）**

| Slot | 位置 | 实例 | 上下文 | 说明 |
| --- | --- | --- | --- | --- |
| `global.status-item` | 状态栏项 | 多 | workspace | 右下角/顶部状态指示 |
| `global.command` | 命令面板项 | 多 | workspace | 可检索执行的命令 |
| `global.toast-action` | 通知动作 | 多 | 无 | 通知上的动作按钮 |

> Slot ID 是宿主与插件之间的**稳定契约**：宿主新增 Slot 必须走语义化版本与公告，删除/改名 Slot 视为破坏性变更，需保留兼容别名过渡。

### 6.2 渲染容器与沙箱

宿主在每个 Slot 位置创建**标准容器组件（`<TgSlot>`）**，按插件声明的 `ui.sandbox` 选择渲染方式。三级容器：

**① Shadow 容器（默认，`sandbox: "shadow"`）**

- 宿主在 Slot 位置渲染 `<tg-slot>` 自定义元素，内部 `attachShadow({ mode: "open" })`。
- 插件提供的入口 HTML、CSS、JS 资源注入 shadow root。
- 样式用 Shadow DOM 天然隔离，不污染主界面；主界面的样式 token 通过 CSS 变量从 App 根节点自然继承（见 6.4）。
- JS 与宿主共享同一个 JavaScript 环境。宿主对桥接调用继续做权限校验，但 Shadow 本身不隔离 `window` / `document`，因此只适用于用户信任的插件。
- 宿主动态注入 `bridge`、`pluginRoot`、初始/变化上下文和卸载登记；旧脚本只读取 `bridge` 时保持兼容。
- 优势：**真正挂载到主 DOM 树**，可参与布局、随 Slot 卸载/恢复、与 React 树共存；框架无关。

**② iframe 容器（可选，`sandbox: "iframe"`）**

- 等价现有 `srcdoc + postMessage` 模式，独立 origin，最强隔离。
- 适用于不信任插件、需要完整浏览器语义（如加载第三方站点）的场景。
- 代价：不融入主 DOM 树，无法与主界面样式/事件直接联动（仅经桥接）。

**③ 原生容器（仅官方，`sandbox: "native"`）**

- 官方内置插件可注册 React 组件直接挂载，获得最佳性能与最紧密集成。
- 该路径需要官方签名，三方不可用；仅用于浏览器、终端等高保真内建面板。

**沙箱分级决策表**：

| 能力诉求 | shadow | iframe | native |
| --- | --- | --- | --- |
| 融入主 DOM 树、随界面布局 | ✅ | ❌ | ✅ |
| 样式隔离 | ✅（Shadow DOM） | ✅（独立文档） | 需自约束 |
| JS 强隔离 | ❌（与宿主共享环境） | ✅ | ❌ |
| 三方可用 | ✅ | ✅ | ❌（需签名） |
| 加载第三方站点/任意网页 | ⚠️ | ✅ | ✅ |

### 6.3 宿主桥接 API（Host Bridge）

无论哪种容器，插件 UI 通过统一桥接访问宿主。桥接是**能力白名单下的异步消息通道**，底层复用现有 `plugin_call` 通道并扩展为双向事件。

```ts
// 插件 UI 沙箱内可用的 bridge 对象（由宿主注入）
interface HostBridge {
  // 调用宿主能力，返回 Promise<string>（JSON 序列化的结果）
  call(method: string, payload: string): Promise<string>;

  // 订阅宿主事件，返回取消函数
  on(channel: string, handler: (payload: string) => void): () => void;
}
```

Shadow 入口脚本除 `bridge` 外还收到以下运行参数；SDK 通过
`getShadowHostRuntime()` 封装这些自由变量，同一份入口可在 iframe 中返回 `null` 并走原有路径：

```ts
interface ShadowHostRuntime {
  root: ShadowRoot;
  context: HostContext;
  onContextChange(handler: (context: HostContext) => void): () => void;
  registerCleanup(cleanup: () => void): void;
}
```

宿主在插件安装、启用或更新后动态执行入口；禁用、更新或卸载时先调用插件登记的
清理函数，再清理事件桥和 Shadow 内容。入口脚本也可以直接返回一个清理函数。

桥接能力（`bridge.call` 可调用的 method）由权限白名单控制，首版提供：

| method 命名空间 | 说明 | 默认权限 |
| --- | --- | --- |
| `plugin.*` | 转发到本插件 WASM 逻辑层（等价现有 `handle-view-message`） | `bridge.call` |
| `session.*` | 读取/操作当前会话（读消息、发消息、切换会话） | `session.read` / `session.write` |
| `tool.*` | Desktop TS 工具调用、结果提交与事件 | `tool.provide` 等 |
| `storage.*` | 读写插件私有数据 | `storage.private` |

事件（`bridge.on`）命名空间与事件接缝一致（见 7.7）：`session.*`、`tool.*`、`lifecycle.*`。

### 6.4 主题与设计 Token

Shadow 容器天然隔离选择器，但 CSS 自定义属性会跨 Shadow 边界继承。为让插件 UI
与宿主视觉一致，Shadow 直接使用 App 根节点的设计 token，主题切换由浏览器自动
重算，不需要上下文订阅。iframe 是独立文档，仍由宿主在挂载及主题、会话切换时
通过 `postMessage` 推送 **hostContext**。Shadow 的初始上下文和
`onContextChange` 仅承担会话等非样式信息：

```jsonc
{
  "type": "tiangong_host_context",
  "theme": "dark",
  "locale": "zh-CN",
  "tokens": {
    "background": "224 71% 4%",
    "foreground": "213 31% 91%",
    "primary": "210 40% 98%",
    "muted": "217.2 32.6% 17.5%",
    "border": "216 34% 17%",
    "radius": "0.5rem",
    "status-success": "142 76% 36%",
    "status-error": "0 84% 60%"
  }
}
```

颜色 token 是 HSL 通道，插件 UI 使用 `hsl(var(--foreground))` 等写法消费；
`--radius` 是可直接使用的长度。iframe 插件把消息中的 token 写入自己的组件根节点，
Shadow 插件无需复制。后续可在这些 token 之上提供前端组件库，降低三方 UI 开发成本。

### 6.5 会话区扩展

会话区是「消息流 + 输入区」组成的对话主界面。插件可：

- **`session.turn-node`**：在消息流中插入自定义节点。例如一个「看板」插件在每轮结束插入一个可交互的总结卡片；一个「图表」插件在工具执行后插入渲染结果。
- **`session.message-item`**：渲染某类消息的自定义视图。例如媒体插件渲染音频波形、文件插件渲染表格预览。缺省作为附加区挂在每条消息下方；贡献声明 `"render": "replace"` 时改为「接管显示」，见 6.5.1。
- **`session.message-action`**：给消息加自定义操作。例如「加入收藏」「转发到看板」。
- **`session.before-input` / `session.after-input`**：输入区上下文 UI。例如快捷指令条、当前工具状态。

普通会话区 Slot 的容器生命周期可与「当前会话 + 消息/轮次」绑定。需要跨会话保活的单例（如 `session.interaction`）切换会话时不重建，宿主只推送新上下文，使插件保留其他会话尚未结束的请求。宿主提供 `session.*` 桥接能力让插件读取消息、触发工具。

#### 6.5.1 消息渲染声明（`meta.render`）

消息可携带插件渲染声明 `meta.render = { plugin, view, data }`，表示该消息的显示由插件 `plugin` 的 `session.message-item` 贡献 `view` 接管。声明只影响显示：模型请求、轮次记账与压缩都不读取它；`data` 序列化后不超过 16KB。

- **来源**：
  - 外部消息入口 `POST /api/v1/messages` 的 `render` 字段（如 Bot 插件为 IM 消息声明卡片视图）；同一请求的 `instruction` 字段保存为只给模型看的指令块，界面不显示。
- **贡献声明**：`{ "slot": "session.message-item", "id": "<view>", "entry": "...", "render": "replace" }`。`render` 仅对 `session.message-item` 合法；`replace` 贡献不再作为附加区挂到每条消息。
- **渲染**：宿主按 `plugin` + `view` 找到 `replace` 贡献后挂载，消息上下文额外带 `message.render = { view, data }`；插件缺失、停用或未声明该视图时，按角色默认渲染（降级可读）。

### 6.6 拓展区扩展与「能力矩阵（App Matrix）」

拓展区即现有右侧 `workspace panel`（`TabKind = 'browser' | 'terminal'`）。引入 `extension.tab` 后，所有「面板类」能力（浏览器、终端、Agent Team、三方 App）统一为**App**，拓展区成为承载 App 的容器。

**核心概念：能力矩阵（App Matrix）**

- **App = 声明了 `extension.tab` 的插件**。一个 App 在矩阵中是一个可打开的入口（图标 + 名称 + 描述 + 打开模式）。
- 顶部不再保留独立的「终端」「浏览器」按钮，**合并为一个「拓展区」按钮**；点击打开拓展区，未打开任何 App 时直接显示 App 矩阵（启动台网格）。
- App 按 manifest 的 `open_mode` 区分两种打开方式：
  - **`singleton`（单例 tab）**：全局至多一个 tab，重复打开聚焦已有实例（Agent Team 即此模式；浏览器/终端为 `multi`）。
  - **`multi`（多 tab）**：每次打开新建一个独立 tab 实例，可并存切换（终端即此模式）。
- 已打开的 App 在矩阵中**标识**：图标带「已打开」角标、多实例显示实例数量、运行中显示状态点。
- 打开 App 后，拓展区顶部显示 tab 栏（可切换实例）+ 一个**启动台按钮**（九宫格图标），点击切回 App 矩阵以便快速打开其他 App。

这样，「新增一种面板」从「改后端命令 + 改前端组件」降级为「写一个声明了 `extension.tab` 的插件」，无需触碰主应用。

### 6.7 拓展区交互模型：App 矩阵

本节细化「能力矩阵」的交互状态与切换规则，作为第 8 章内置能力迁移的 UI 前置设计。

#### 6.7.1 顶部入口收敛

- 删除 `StatusPanel` 中的独立终端（`TerminalSquare`）与浏览器（`Globe`）两个按钮，替换为单个「拓展区」按钮（`Grid3x3` / `LayoutGrid` 图标）。
- 「拓展区」按钮承担原有两个按钮的状态聚合：只要当前会话存在任一已打开的 App tab，按钮即高亮；存在「agent 使用中」的 App 时，按钮显示使用中绿点（原 `browserAgentActive` / `terminalAgentActive` 语义泛化为「App 使用中」）。

#### 6.7.2 拓展区三态

拓展区（右侧面板）存在三种稳定状态：

1. **关闭态**：拓展区收起，只保留顶部「拓展区」按钮。
2. **矩阵态（启动台）**：拓展区打开且未聚焦任何 App 时，显示 App 矩阵网格；列出所有已安装 App，标注打开状态。
3. **App 态**：拓展区聚焦某个 App 实例，显示 App 内容 + tab 栏 + 启动台按钮。

状态切换规则：

| 当前态 | 动作 | 结果 |
| --- | --- | --- |
| 关闭态 | 点「拓展区」按钮 | 进入矩阵态（无已打开 App）或上次聚焦的 App 态（有已打开 App） |
| 矩阵态 | 点某个 App | 打开/聚焦该 App，进入 App 态 |
| 矩阵态 | 点「拓展区」按钮或关闭 | 收起，进入关闭态 |
| App 态 | 点启动台按钮 | 切回矩阵态 |
| App 态 | 切换 tab | 在同一 App 的多实例间或不同 App 间切换 |
| App 态 | 关闭当前 tab | 若还有其它 tab 则切到相邻 tab；否则回到矩阵态 |

#### 6.7.3 App 打开与实例管理

- **单例 App**：矩阵中点击若未打开则新建 tab 并聚焦；已打开则直接聚焦其 tab。关闭该 tab 即释放实例。
- **多实例 App**：矩阵中每次点击都新建 tab；tab 栏中以「App 名 + 序号/标题」区分，可独立关闭。实例标题由 App 经桥接上报（如终端目录、页面标题）。
- **会话绑定**：App 实例沿用现有浏览器/终端的会话路由——同一 App 在不同会话各自维护实例集合；切换会话时，拓展区显示当前会话的实例。

#### 6.7.4 矩阵中的标识

矩阵中每个 App 卡片显示：

- **已打开标识**：单例 App 显示「已打开」；多实例 App 显示实例数量徽标（如 `×3`）。
- **运行态**：App 上报 `running` 时显示状态点（如终端有命令运行、浏览器有 agent 导航）。
- **打开模式提示**：图标角落以图形或 tooltip 标识「单例 / 多实例」。

#### 6.7.5 启动台按钮

- App 态下，拓展区 tab 栏最左侧固定一个启动台按钮（九宫格图标）。
- 点击进入矩阵态；矩阵态下该按钮切换为「返回」语义或高亮。
- 启动台按钮是「快速回到矩阵打开其他 App」的唯一入口，替代旧版「顶部两个独立按钮」的心智模型。

#### 6.7.6 与现有实现的关系

- `TabKind = 'browser' | 'terminal'` 泛化为 `{ kind: 'plugin', pluginId, contributionId, instanceId }`；内置浏览器/终端作为官方 App 注册，`instanceId` 用于多实例路由。
- `MainApp.openWorkspacePanel(kind)` 重构为 `openExtension(matrix: boolean | { appId, contributionId })`；`browserAgentActive` / `terminalAgentActive` 收敛为按 App 维度维护的「使用中」集合。
- `TabsContainer` 泛化为 App tab 容器，启动台按钮与矩阵视图作为拓展区的两个固定组件。

---

（下接第 7 章 能力接缝详解）
---

## 7. 能力接缝（Seam）详解

### 7.1 工具接缝（Tool）

沿用现有 `plugin.wit` 的 `tool-specs` / `handle-tool`，语义不变。本阶段不为审批征询扩展 WIT 接口或工具元数据。

### 7.2 提示词接缝（Prompt）

沿用现有 `prompt-sections`，语义不变。

### 7.3 生命周期接缝（Lifecycle）

沿用现有 `set-workspace`、`on-config-updated`、`on-session-ready`、`on-turn-started/finished`、`on-session-ended`。保持不变，保证现有 Memory 等插件无需改动。

### 7.4 UI 接缝（UI）

见第 6 章。契约由三部分组成：

- **贡献声明**：manifest 的 `ui.contributions`（Slot + 资源入口）。
- **资源获取**：宿主按需调用 `open-view(contribution-id)` 取入口 HTML、`get-view-resource(path)` 取 CSS/JS/图片（沿用现有 WASM 接口并扩展为支持多 Slot）。
- **通信**：宿主桥接 `bridge.call` / `bridge.on`（见 6.3）。

### 7.5 Desktop TypeScript 工具桥

Desktop 纯 TypeScript 插件可直接在 schema v2 manifest 中声明 `tools` 与 `prompt`，
不包含 WASM，也不扩展 `plugin.wit`。宿主把工具注册到现有 Core 工具流水线，
并使用以下中立协议把调用交给插件页面：

1. `tool.requested` 提供权威 `invocation_id`、会话、工具调用 ID、工具名、参数和创建时间。
2. 插件通过 `tool.resolve` 提交完整 Tool Result，可标记 `answered`、`expired` 或 `cancelled`。
3. `tool.closed` 通知最终闭合状态；错插件、迟到和重复提交均被宿主拒绝。
4. 插件禁用、卸载、页面销毁或 turn 取消时，宿主取消仍在等待的调用。

该桥只透传工具名、参数和结果，不识别具体插件、工具或业务类型。

### 7.6 审批与用户征询插件

默认 `plugins/tiangong-plugin-interaction` 在上述通用工具桥上声明 `request_user`，并独立实现
approval、confirm、choice、multi_choice、input、form 六类参数解析、界面和结果。
插件按调用创建时间独立执行 15 秒用户时限，宿主只保留 20 秒通用工具执行上限。

审批结果是插件收集后返回给 Agent 的普通工具结果。Agent 自行决定后续步骤；Core
和公共运行时不解释决定、不放行工具，也不保存征询时限、挑战或授权。完整契约见
`docs/plugin-harness/interaction-model-redesign.md`。

### 7.7 事件接缝（Event）

**目标**：让插件订阅宿主事件流，从「被动响应生命周期」升级为「主动感知宿主状态」。

**现状**：插件只有 `feedback.emit-stream-event`（单向输出）和有限生命周期回调，无法订阅会话和工具执行等事件。

**契约设计**：

- 宿主定义**事件命名空间**：`session.*`（会话创建/更新/标题/消息）、`tool.*`（工具调用与结果）、`lifecycle.*`（轮次/会话钩子）、`config.*`（配置变更）。
- 插件在 manifest `capabilities.events` 声明订阅的命名空间（最小授权）。
- 运行时经桥接 `bridge.on(channel, handler)` 订阅，宿主按命名空间路由推送，插件卸载自动退订。

### 7.8 存储接缝（Storage）

沿用现有 sidecar 数据目录机制，补充：

- 每个插件默认获得**私有数据目录**（`storage.private`，等价现有 `TIANGONG_PLUGIN_DATA_DIR`）。
- 共享应用存储（`storage.app`）仍需官方签名（等价现有 `app-storage.read`）。
- UI 沙箱内经桥接 `storage.*` 读写私有数据（经逻辑层转发到 WASM/sidecar 落地），避免沙箱直接接触文件系统。

---

（下接第 8 章 内置能力插件化迁移）
---

## 8. 内置能力插件化迁移

本节描述如何把现有内建能力改写为「官方插件」。目标：**三方能开发的形态，官方内建功能同样用该形态实现**，消除特权通道。

### 8.1 嵌入式浏览器（Browser）

现状：`plugin:browser|*` 硬编码 Tauri 命令 + `BrowserTabContent` 专用前端组件 + `tiangong-plugin-browser` 原生 crate。

迁移：

- 拆为**官方「浏览器」插件**：声明 `extension.tab`（`sandbox: "native"`，保留高保真面板；`open_mode: "multi"`）+ 工具接缝（`browser_open`、`browser_navigate`、`browser_eval` 等现有工具）+ 事件接缝（页面/标签状态）。
- 前端 `BrowserTabContent` 变为该插件的原生容器实现；`plugin:browser|*` 命令迁移为该插件经桥接 `tool.invoke` 调用的内部通道。
- 迁移后，三方可仿照浏览器插件开发自己的「面板类」能力（如代码地图、数据库浏览器、看板）。

### 8.2 嵌入式终端（Terminal）

现状：`plugin:terminal|*` 命令 + `TerminalTabContent` + `tiangong-plugin-terminal` crate。

迁移：

- 拆为官方「终端」插件：`extension.tab`（`sandbox: "native"`；`open_mode: "multi"`）+ 工具接缝（`terminal_run`、`terminal_send` 等）+ 事件接缝（`terminal_data` 等）。
- 终端数据注入复用现有事件接缝，替代当前散落的后端推送。
- 迁移后，会话内嵌终端成为一个可替换插件，三方可开发自定义「执行环境」面板（如 SQL 控制台、REPL）。

### 8.3 Agent Team（Agent 协作 / Bots）

现状：`tiangong-bots` crate + `BotPanel`/`BotFormDialog` 等前端组件，子 agent 调度在 Core 多代理协调层。

迁移：

- 拆为官方「Agent Team」插件：`sidebar.panel` 或 `extension.tab`（团队面板）+ 工具接缝（创建/分派/聚合子 agent）+ 事件接缝（子任务状态）。
- 子 agent 调度核心保留在 Core（作为 Seam 的宿主能力），UI 与编排策略走插件。
- 迁移后，三方可替换 Agent Team 的界面，或新增「工作流编排」「审批式多代理」等协作形态。

### 8.4 操作审核（Approval）

当前实现：Agent 在需要用户意见时调用 `request_user(kind=approval)`，Desktop
`interaction` 插件显示操作框，并在 15 秒内返回普通工具结果。

- 用户选择只作为 Tool Result 返回 Agent，由 Agent 决定后续步骤。
- Core 和公共运行时不维护审批计时、挑战、授权或放行状态。
- 三方可用相同的 TS 工具协议替换交互处理器界面和结果表达。

### 8.5 用户交互（选择 / 填写）

`request_user` 同时支持 confirm、choice、multi_choice、input 和 form。插件负责参数
解析、界面、15 秒时限及结果生成；Agent 通过普通 Tool Result 取得用户输入。

### 8.6 三方 App 拓展（Third-party App Extension）

**目标**：让三方应用作为一个「插件」整体接入天工，而不仅是贡献工具或面板。

设计：引入**「应用形态（App Profile）」**——一个插件可以声明自己是独立应用：

```jsonc
{
  "ui": {
    "contributions": [
      {
        "slot": "extension.tab",
        "id": "app-main",
        "entry": "index.html",
        "open_mode": "singleton",  // 单例 tab；multi 则每次打开新建实例
        "app": true                // 标记为应用形态：可独立窗口/全屏运行
      }
    ]
  },
  "capabilities": {
    "tools": true,
    "events": ["session.*"]
  }
}
```

- 应用形态插件获得独立窗口/全屏运行能力（经宿主桥接 `app.*` 申请）。
- 它与普通插件的区别只在 `app: true` + 更宽的权限申请，仍走同一 manifest、同一沙箱、同一桥接；同样进入拓展区的**能力矩阵**，遵循 singleton/multi 打开规则。
- 典型场景：把「任务管理 App」「代码评审 App」「知识库 App」作为天工内的应用接入，同时向 Agent 暴露工具。

---

## 9. 三方开发体验

### 9.1 开发模型

支持两种编写模型，产出同一套插件制品：

1. **UI 优先（JS/TS）**：用任意前端技术编写 UI，逻辑简单时直接在沙箱内用 JS 完成，经桥接调用宿主能力；需要原生/重逻辑时再挂 WASM 或 sidecar。
2. **逻辑优先（Rust → WASM）**：沿用现有 `plugin.wit` 编写工具/生命周期，UI 通过资源目录声明贡献到 Slot。

### 9.2 SDK 与脚手架

- **`@tiangong/plugin-sdk`**：TypeScript 类型定义（HostBridge、Slot、Seam、上下文）、桥接客户端、事件订阅封装、宿主设计 token 类型。
- **`@tiangong/plugin-ui-kit`**（可选）：与主界面同源的 React/Vue/Web 组件（按钮、表单、卡片），基于 token 自动适配主题。
- **`create-tiangong-plugin`**：脚手架，一键生成目录结构、manifest、WASM 占位、UI 入口、开发脚本。
- **WIT 绑定生成**：为 JS 逻辑层提供 `tiangong-plugin` 的 JS 绑定（或经 sidecar 桥接），降低「想写逻辑但不想用 Rust」的门槛。

### 9.3 开发调试

- 本地目录热加载（沿用现有 `import_local_plugin`，扩展为监听资源变化热刷新 UI）。
- 沙箱内保留 `bridge.debug` 日志通道，宿主开发者工具可查看插件 UI 的桥接消息与错误。
- 提供插件模板仓库 + 官方示例（一个「看板」插件演示所有 Slot 与 Seam）。

### 9.4 打包与分发

- 统一制品结构：`manifest + WASM + 资源目录 + 可选 sidecar`，沿用现有 `xtask build-plugin` 与 OSS 目录分发。
- 签名策略沿用现状：纯 WASM/Shadow 插件无需签名；`native` 容器、sidecar、敏感权限需官方签名。
- 新增插件市场/目录项展示 Slot 与权限，用户安装前可见能力与授权范围。

---

（下接第 10 章 安全与权限模型）
---

## 10. 安全与权限模型

安全分层沿用现有「签名 + 权限声明 + 运行时中立」体系，按新形态扩展：

1. **沙箱分级**：`shadow` / `iframe` / `native` 三级（见 6.2）。三方默认 `shadow`，可用 `iframe` 自降为强隔离；`native` 需官方签名。
2. **最小权限**：`permissions` 逐项声明，导入时校验；敏感项（共享存储、原生容器、sidecar）不接受仅靠 manifest 自授。
3. **桥接白名单**：`bridge.call` 的 method 按权限命名空间放行，未知 method 拒绝并记录；`bridge.on` 的事件按 manifest `capabilities.events` 放行。
4. **资源与脚本边界**：插件资源经 `get-view-resource` 按白名单路径读取，iframe 提供脚本强隔离；Shadow 与宿主共享 JavaScript 环境，当前允许普通三方声明，但安装来源必须可信，不能把它视为安全沙箱。
5. **插件时限**：交互处理器自行在 15 秒后返回超时结果；宿主 20 秒通用上限只处理插件崩溃或失联。
6. **可逆卸载**：插件卸载/禁用时撤销全部注册（Slot、工具、事件订阅、sidecar 进程），不残留 UI 与状态。
7. **宿主中性**：`tiangong-plugin-runtime` 不感知具体插件业务，仅转发不透明负载；UI 桥接同样只做鉴权与透传，不解析业务 JSON。

---

## 11. 兼容与迁移策略

**兼容原则：现有 WASM 插件零改动继续运行。**

- `plugin.wit` 的 `plugin` / `plugin-ui` 接口保留；`plugin-ui.contributions` 映射为 `settings.plugin-page` Slot，`open-view` / `handle-view-message` 映射为桥接 `plugin.*`。
- `schema_version: 1` 的 `plugin.json` 按旧规则解析，`ui` 缺省等价于「仅设置页」。
- 现有 sidecar 签名、OSS 分发、导入/升级/回滚流程不变。

**迁移节奏（建议分阶段）：**

1. **阶段 0：接缝地基**。在 Runtime/前端建立 Slot Registry、Seam Hub、Host Bridge 基础协议；`settings.plugin-page` 作为第一个 Slot 落地，验证「旧插件经新桥接渲染设置页」。
2. **阶段 1：UI 接缝与能力矩阵**。开放 `extension.tab`、`session.*` 等 Slot；实现 Shadow/iframe 容器与桥接白名单；收敛顶部入口为「拓展区」按钮，落地 App 矩阵、启动台按钮与 singleton/multi 打开。
3. **阶段 2：内置插件化**。浏览器、终端迁移为官方 `extension.tab` 插件；Agent Team 迁移为官方面板插件。
4. **阶段 3：用户征询插件**。以 Desktop TS `request_user` 工具替换内置审批与表单路径。
5. **阶段 4：三方体验**。SDK、脚手架、UI Kit、示例、市场展示能力/权限；开放 App 形态。

每阶段产出可独立交付、可回滚，且不破坏上一阶段插件。

---

## 12. 后续拓展方式

设计上预留的扩展机制：

1. **新增 Slot**：宿主在 Slot Registry 登记新锚点（如 `session.inline-tool`、`global.notification`），即对三方开放新挂载位置，无需改插件协议。Slot 版本化，删除/改名保留兼容别名。
2. **新增 Seam**：新的能力类别（如「模型适配 Seam」「检索/知识库 Seam」「工作流 Seam」）作为新 Seam 契约加入，内置实现与三方实现同构，不影响既有 Seam。
3. **新增桥接能力**：`bridge.call` 按命名空间扩展（如 `rag.*`、`model.*`、`workflow.*`），配合权限声明即可开放，无需改容器协议。
4. **容器类型扩展**：当前三级容器可再扩展（如「WebView 容器」用于嵌入式浏览器类面板的更强集成），Slot 层面对插件透明。
5. **App 形态深化**：从「应用形态插件」进一步演进为「插件即应用」的完整独立窗口/生命周期管理，为三方 App 市场铺路。能力矩阵从「固定网格」演进为「可分组、可搜索、可固定常用 App」的启动台，并支持 App 拖拽排序与快捷键唤起。
6. **事件接缝深化**：从「订阅只读事件」演进为「事件可被插件消费并产出新事件」，形成可编排的插件链（在沙箱安全约束内）。

这些扩展都基于「接缝 + 挂载点 + 桥接」的稳定三层抽象，新增能力不需要推翻既有插件生态。

---

## 13. 非目标

- **不引入 Cordis / 不在天工内复刻 Cordis**：仅借鉴其插件组织思想。
- **不实现通用插件市场平台**：短期沿用现有 OSS 静态目录分发，仅增强目录项的能力/权限展示。
- **不在本次重写 Core 的 Agent Loop**：`request_user` 作为普通插件工具接入现有工具流水线。
- **不追求跨语言逻辑运行时**：逻辑层仍以 WASM Component 为统一契约；JS 逻辑层属可选增强，不替代 WASM。
- **不做任意网页内嵌浏览器的通用沙箱**：iframe/WebView 容器的安全边界沿用现有浏览器插件策略，不新增通用站点沙箱承诺。

---

## 14. 里程碑

| 里程碑 | 内容 | 验收标准 |
| --- | --- | --- |
| M0 | 接缝地基：Slot Registry + Seam Hub + Host Bridge 协议 | 旧插件经 `settings.plugin-page` 走新桥接渲染，回归通过 |
| M1 | UI 接缝：Shadow/iframe 容器 + `extension.tab`/`session.*` 开放 | 三方示例插件可挂载拓展区 tab 与会话区节点 |
| M2 | 拓展区能力矩阵：顶部入口收敛为「拓展区」按钮 + App 矩阵 + 启动台 + singleton/multi 打开 | 终端/浏览器作为 App 在矩阵中打开，单例/多实例切换正确 |
| M3 | 内置插件化：浏览器/终端/Agent Team 迁移为官方插件 | 三个能力以插件形态运行，行为不劣于现状 |
| M4 | 用户征询插件：审批 + 交互落地 | 审批/表单由 Desktop TS 插件处理，结果作为普通 Tool Result 返回 Agent |
| M5 | 三方体验：SDK/脚手架/UI Kit/示例/市场展示 | 开发者用脚手架可完成「面板 + 工具 + 审批」全链路插件 |

---

## 15. 附录：与现有 WIT 的关系

现有 `plugin.wit` 的接口在新形态中的归属：

| 现有 WIT 接口 | 新归属 |
| --- | --- |
| `describe` / `tool-specs` / `handle-tool` | 工具接缝（不变） |
| `prompt-sections` | 提示词接缝（不变） |
| `set-workspace` / `on-config-updated` / `on-session-ready` / `on-turn-*` / `on-session-ended` | 生命周期接缝（不变） |
| `contributions` / `open-view` / `get-view-resource` / `handle-view-message` | UI 接缝（映射到 Slot + Host Bridge，扩展为多 Slot） |
| `clock.now-millis` | 保留（宿主导入） |
| `sidecar.invoke` | 存储/原生能力接缝（保留） |
| `feedback.emit-stream-event` | 事件接缝的「输出」侧（扩展为双向订阅） |

Desktop TS 工具、审批与用户征询不修改 `plugin.wit`。新增声明位于 schema v2
`plugin.json`，调用通过宿主 `tool.*` 桥完成；现有 WASM 插件接口和二进制保持不变。
