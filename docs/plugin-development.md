# 天工插件开发指南

本文说明当前插件化试开发阶段如何创建、构建和本地导入 UI、TypeScript 与
WASM 插件。WASM 宿主接口位于
[`plugin.wit`](../crates/tiangong-plugin-runtime/wit/tiangong/plugin.wit)，UI 运行时
类型位于 [`plugins/sdk`](../plugins/sdk)。

## 开发流程总览

插件开发分两个循环：**本地开发循环**（改代码 → 打包 → 导入验证）与后续的
**分发流程**（CI 打包、OSS 发布）。日常开发只需要本地循环。

```text
选择形态 → 开发 → 打包 → 本地导入 → 验证 → （迭代） → 分发（后续）
```

### 1. 选择插件形态

| 形态 | 适用 | 参考工程 |
| --- | --- | --- |
| 纯 UI 插件（无 WASM） | 面板、工具页、输入区动作，仅需宿主桥接 | `plugins/templates/ui-app` |
| Desktop TypeScript 工具插件 | 带 UI 的工具提供器、审批与用户征询 | `plugins/tiangong-plugin-interaction`（Vue 3 + Vite） |
| WASM 逻辑层插件（v1/v2） | 工具、提示词、生命周期、sidecar | `plugins/tiangong-plugin-prompt` 等 |
| v2 混合（逻辑层 + UI 挂载） | UI 需要 WASM 或原生 sidecar 能力 | `plugins/screenshot-input` |

脚手架：`cargo run -p xtask -- new-plugin <id>` 生成纯 UI 最小骨架
（不含交互权限，按需在 manifest 声明）。

### 2. 开发

- 纯 UI 插件：标准前端工程（推荐 Vue 3 + Vite，见参考工程结构），
  桥接与类型用本地 SDK `plugins/sdk`（`@tiangong/plugin-sdk`）。
- 当前本地导入流程按清单复制 UI 入口，工程化插件应产出**自包含单文件**
  （JS/CSS 内联，如 `vite-plugin-singlefile`），避免安装后缺少关联资源；iframe
  的 `srcdoc` 入口同样不能依赖原开发服务器路径。
- WASM 插件：Rust + WIT，见「实现 WASM Component」。

### 3. 打包（开发期）

统一命令产出可导入的插件包目录（`release/`：plugin.json + 构建产物，
不含源码与 node_modules），打包即校验清单与入口就位：

```bash
# 纯 UI 插件（工程目录内）
yarn package

# WASM 插件（仓库根目录）
cargo run -p xtask -- build-plugin <id>
```

### 4. 本地导入与验证

天工「设置 → 插件管理 → 导入本地插件」选择 `release/`（纯 UI）或构建输出
目录（WASM 插件）——走正式导入流程（清单校验 → 事务安装 → 注册表加载），
完整验证真实安装链路。验证要点：

- 设置页贡献正常渲染、拓展区 App 可打开；
- 桥接调用（storage / plugin.*）与主题跟随正常；
- 修改代码后重新 `yarn package`，对已装插件热加载或重新导入新版本。

### 5. 分发（后续）

CI 打包（GitHub Actions）与 OSS 目录发布属后续发布流程，开发期不涉及；
三方插件亦可通过 `TIANGONG_PLUGIN_CATALOG_URL` 指向自建静态目录（规划中）。

---

## 插件组成

一个可安装插件至少包含 `plugin.json` 及清单引用的运行制品。schema v2 纯 UI
插件可以不含 WASM；WASM、sidecar 和 UI 也可按需要组合。原生 sidecar 仅对带
有效天工官方签名的发布包开放：

```text
WASM Component
    |  通用 Host 接口
    v
天工插件运行时
    |  通用认证与 JSON Lines 协议
    v
可选 sidecar
```

- UI 入口负责界面与 Desktop TypeScript 行为，可通过宿主桥接访问获准能力。
- WASM 负责跨入口工具、提示词注入和生命周期等逻辑能力。
- sidecar 负责数据库、文件系统、原生库或长时间运行的业务。
- 天工只负责加载、资源限制、sidecar 进程管理和消息转发，不理解插件业务负载。
- WASM 与 sidecar 之间的私有业务协议由插件自己维护。

## WASM Runtime 公共中立约束

`tiangong-plugin-runtime` 是所有插件共享的基础设施，后续开发必须保持公共、中立和业务无关。Runtime 只能提供任意插件都能复用的加载、隔离、资源限制、生命周期转发、通用反馈、sidecar 传输和制品管理能力。

禁止在 Runtime、公共 WIT 或通用 sidecar 协议中加入：

- 具体插件 ID、工具名、操作名或基于插件身份的条件分支；
- 具体插件的请求、响应、事件、配置和数据结构；
- 具体插件的数据目录、兼容路径、迁移规则或生命周期策略；
- 仅为某个插件服务的便捷方法、错误码、状态字段或特殊传输流程；
- 对 JSON 业务负载内容的解析、改写、筛选或语义判断。

单个插件需要的功能必须放在以下位置之一：

- 插件 WASM：工具行为、提示词、生命周期编排和宿主通用能力调用；
- 插件私有协议：WASM 与 sidecar 共用的业务操作、请求和响应；
- 插件 sidecar：数据库、原生库、业务处理和插件自己的兼容逻辑。

确需扩展 Runtime 时，改动必须同时满足：

1. 不知道调用方是哪一个插件也能正确工作；
2. 接口和实现不引用任何具体插件的名称或类型；
3. 至少能合理服务两类不同插件，而不是把单插件逻辑包装成“通用”接口；
4. Runtime 只转发不透明负载或通用数据，不解释业务含义；
5. 新能力有独立的通用测试，插件自己的行为测试留在插件目录。

代码审查发现单插件需求进入 Runtime 时，应退回插件侧实现。不得以临时兼容、示例插件或当前只有一个使用方为理由增加特化处理。

## 开发环境

当前仓库使用 Rust 2024 edition，WASM 目标为 `wasm32-wasip2`：

```bash
rustup target add wasm32-wasip2
cargo check -p tiangong-plugin-runtime
```

需要修改天工前端时使用 yarn：

```bash
yarn --cwd frontend install
yarn --cwd frontend dev
```

## 目录结构

建议把同一插件的源码放在一个目录中：

```text
plugins/ui-example/
├── plugin.json
├── package.json
├── index.html
├── src/
│   ├── main.ts
│   └── App.vue
├── scripts/package.mjs
└── vite.config.ts

plugins/tiangong-plugin-example/
├── plugin.json
├── protocol/          # 可选，WASM 与 sidecar 共用的私有协议
├── wasm/
└── sidecar/           # 可选
```

最终导入的是构建后的完整目录，不是源码目录。纯 UI 插件的发布目录通常为：

```text
release/
├── plugin.json
└── dist/
    └── index.html
```

纯 WASM 插件无需签名；官方 sidecar 插件还必须包含签名清单：

```text
example-plugin/
├── plugin.json
├── example_plugin.wasm
├── example-sidecar      # 清单声明 sidecar 时必须提供
├── release.json         # 官方 sidecar 插件必须提供
└── release.json.sig     # 官方 sidecar 插件必须提供
```

`runtime`、`logs` 和 `data` 由天工管理，不需要放进导入目录。

## 插件清单

`plugin.json` 使用以下格式：

```json
{
  "schema_version": 1,
  "id": "example",
  "version": "0.1.0",
  "wasm": {
    "binary": "example_plugin.wasm"
  },
  "sidecar": {
    "binary": "example-sidecar",
    "startup_timeout_ms": 15000,
    "request_timeout_ms": 30000
  },
  "permissions": ["sidecar.invoke"]
}
```

约束如下：

- `schema_version: 1` 用于既有 WASM 插件；声明 UI Slot 或纯 UI 形态时使用 `2`。
- `id` 只能包含 ASCII 字母、数字、点、下划线和连字符。
- `version` 必须是语义版本，例如 `0.1.0`。
- `wasm.binary` 和 `sidecar.binary` 必须是插件目录内的安全相对路径。
- WASM `describe()` 返回的 ID、版本必须与清单完全一致。
- 声明 sidecar 时，插件权限必须包含 `sidecar.invoke`，且发布包必须带有效官方签名。
- `model-config.read` 允许 sidecar 取得包含模型配置的应用存储根目录，仅用于需要直接调用模型的官方插件。
- `app-storage.read` 允许 sidecar 取得应用共享存储根目录，用于兼容 Index、Scheduler、Skill、MCP 等既有业务数据；该权限同样只接受官方签名授权。
- `model-config.read` 与 `app-storage.read` 都是敏感权限，不得仅通过修改 `plugin.json` 自行获取。
- Windows 制品在清单名称后自动使用 `.exe` 后缀，清单本身不要重复添加平台后缀。

不需要 sidecar 的插件应删除整个 `sidecar` 字段，并可将 `permissions` 留空。

## 实现 WASM Component

插件必须实现 [`tiangong-plugin` world](../crates/tiangong-plugin-runtime/wit/tiangong/plugin.wit)，包括 `plugin` 与 `plugin-ui` 两组导出。即使插件没有页面，也需要实现 `plugin-ui` 并返回空贡献列表。

最小 crate 配置：

```toml
[package]
name = "tiangong-plugin-example-wasm"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.46"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

在本仓库开发时直接引用权威 WIT：

```rust
wit_bindgen::generate!({
    path: "../../../tiangong-plugin-runtime/wit/tiangong/plugin.wit",
});
```

实现时需要关注这些导出：

- `describe`：返回稳定的插件 ID、显示名称和版本。
- `tool-specs`、`handle-tool`：声明并执行工具。
- `prompt-sections`：向当前会话提示词添加插件内容。
- `set-workspace` 和会话、轮次生命周期：接收宿主只读状态。
- `contributions`：声明设置页入口。
- `open-view`、`get-view-resource`、`handle-view-message`：提供页面和双向消息。
- `shutdown`：释放 WASM 内部状态。

`tool-specs` 与 `prompt-sections` 首次采集后按插件顺序保存到会话，并由 Core 的 `PreparedPlugins` 使用。普通 turn 和 Core 重建恢复这份声明，不按工具的临时健康状态重新增删。压缩成功或清理上下文时重新采集，与摘要和系统提示一起保存；读取失败沿用该插件的旧声明，不能把错误伪装成空列表。执行权限和可用性仍在调用入口检查。

工具不可用时，`handle-tool` 返回明确的失败结果。sidecar 检测到恢复后，可用现有 Notification 帧发送 `runtime.tools_recovered`，payload 为 `{"tools":["模型可见的工具名"]}`。宿主通过插件反馈通道向相关的活动 Agent 追加 `plugin_availability` 信息，不修改 system prompt/tools，也不唤醒已结束 turn。后台恢复事件应只在不可用到可用的转换时发送，避免重复通知。

宿主导入目前包括：

- `clock.now-millis`：读取真实时间。
- `sidecar.invoke`：向当前插件绑定的 sidecar 转发操作名和 JSON。
- `feedback.emit-stream-event`：向当前 turn 发送通用 `StreamEvent` JSON；Runtime 只验证公共事件结构，不理解具体插件业务。

完整实现可参考 [Memory WASM](../plugins/tiangong-plugin-memory/wasm/src/lib.rs) 和它的 [绑定入口](../plugins/tiangong-plugin-memory/wasm/src/bindings.rs)。

构建命令：

```bash
cargo build -p tiangong-plugin-example-wasm --target wasm32-wasip2 --release
```

将 `target/wasm32-wasip2/release/` 下生成的 `.wasm` 文件复制到完整插件目录，并确保名称与清单一致。

## 接入 sidecar

sidecar 启动时由天工注入以下环境变量。前三项始终可用；共享存储根只有签名权限授权后才会注入：

| 环境变量 | 内容 |
| --- | --- |
| `TIANGONG_PLUGIN_ID` | 插件 ID |
| `TIANGONG_PLUGIN_VERSION` | 插件发布版本 |
| `TIANGONG_PLUGIN_ENDPOINT` | sidecar 写入 endpoint 的文件路径 |
| `TIANGONG_PLUGIN_DATA_DIR` | 插件独立数据目录 |
| `TIANGONG_STORAGE_ROOT` | 天工应用共享存储根；仅官方签名的 `model-config.read` 或 `app-storage.read` 插件可获得 |

sidecar 必须：

1. 监听本地 TCP 端口并生成短期认证 Token。
2. 将 host、port、PID、Token 写入指定 endpoint 文件。
3. 使用运行时 `0.1.0` JSON Lines 帧协议完成认证、请求和响应。
4. 实现 `runtime.handshake`，返回插件 ID、插件版本、传输协议版本、业务协议版本和运行状态。
5. 保证握手中的插件版本与 `plugin.json`、WASM `describe()` 一致。

协议类型见 [`protocol.rs`](../crates/tiangong-plugin-runtime/src/protocol.rs)，进程行为见 [`sidecar.rs`](../crates/tiangong-plugin-runtime/src/sidecar.rs)。可运行示例见 [Memory sidecar](../plugins/tiangong-plugin-memory/sidecar/src/main.rs)。

## 本地导入与调试

完成插件目录后：

1. 启动天工并打开“设置”。
2. 进入“插件管理”。
3. 点击“导入本地插件”图标。
4. 选择包含 `plugin.json` 的完整插件目录。

导入过程会复制制品，不会直接从开发目录运行。天工随后依次检查：

- 清单格式和语义版本；
- 清单声明的 WASM 与当前平台 sidecar 是否为实际文件；
- `release.json` 与 `release.json.sig` 是否成对存在；
- 带 sidecar 时是否为有效官方签名，签名权限是否与清单一致，签名覆盖的清单、WASM 和 sidecar 哈希是否匹配；
- WASM 是否能实例化；
- WASM 描述符与清单 ID、版本是否一致；
- sidecar 是否能启动并通过身份、版本和协议握手。

全部检查成功后才会切换插件。导入失败时继续使用原版本。

版本规则：

- 未安装的插件可以直接导入。
- 同版本允许重新导入，便于本地开发调试。
- 更高版本按升级处理，并保留一个可回滚版本。
- 更低版本会被拒绝，需要使用“回滚”或修改插件版本。
- 替换时保留当前插件的 `runtime`、`logs` 和 `data`；开发目录中的这些内容不会被导入。

本地目录由用户主动选择，但本地选择不能绕过原生代码边界：未签名纯 WASM 可以导入；任何带 sidecar 的插件都必须携带有效官方签名。第三方开发者应使用纯 WASM 和受限 Host 接口，不应获得官方签名私钥。OSS 安装还会先校验目录声明的 SHA-256，再执行统一签名验证。

## Memory 参考构建

Memory 插件同时包含私有协议、WASM 和 sidecar，可用一条命令完成检查、构建、本地部署和 OSS 制品生成：

```bash
TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH=~/.tiangong/keys/plugin-signing.key \
  cargo run -p xtask -- build-plugin memory
```

该命令会生成 `release.json` 与 `release.json.sig`。签名私钥只应保存在天工官方发布环境或本地受保护的开发密钥目录，绝不能写入插件包或提交到仓库；签名密码可通过 `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD` 提供。

生成的 OSS 上传目录位于：

```text
target/plugin-dist/
```

Memory 的目录和构建说明见 [`plugins/tiangong-plugin-memory/README.md`](../plugins/tiangong-plugin-memory/README.md)。

## 常见问题

### 提示清单与组件 ID 或版本不一致

同时更新 `plugin.json` 和 WASM `describe()`。带 sidecar 时还要更新握手返回的插件版本。

### 提示当前平台 sidecar 不存在

确认完整插件目录包含当前操作系统和架构可执行的 sidecar。Windows 文件名应带 `.exe`。

### sidecar 启动后仍显示异常

检查插件目录下的 `logs/sidecar.log`，并确认 endpoint 文件由当前进程生成、握手协议为 `0.1.0`。

### 导入同版本后代码没有变化

确认选择的是构建制品目录而不是源码目录，并先重新构建 WASM 或 sidecar。导入成功后插件会立即替换，无需重启天工。

## GitHub Actions 发布到 OSS

短期官方插件只通过 OSS 静态目录独立发布，不建设插件服务平台。独立工作流位于 [`.github/workflows/publish-plugins.yml`](../.github/workflows/publish-plugins.yml)，可手动填写一个插件 ID，或使用 `plugin/<plugin-id>/v<version>` 标签触发。工作流不维护插件下拉白名单，而是让仓库构建配置核对该 ID 是否对应 `plugins/` 下的 `plugin.json` 与声明的制品（WASM、protocol、sidecar、UI）；每次只构建、签名和上传所选插件。发布形态由 `plugin.json` 决定：纯 UI 插件（如 `interaction`、`browser`）只发布单文件页面，跳过 WASM 构建，无 sidecar 则不生成签名清单；带 sidecar 的插件（如 `terminal`）仍要求官方签名并在三平台矩阵构建原生二进制。

日常发布一次只触发一个插件，并等待任务结束后再发布下一个。首次集中发布多个插件时每批最多触发两个，确认本批任务全部结束后再触发下一批；不要同时推送大量插件标签，因为共享发布锁只负责串行写入，等待中的任务可能被后续任务取消。

仓库需要配置以下 GitHub Actions Secrets：

- `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY`：插件专用签名私钥内容，不得与应用更新私钥复用。
- `TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD`：插件签名私钥密码；无密码时可留空。
- `ALIYUN_OSS_ACCESS_KEY_ID`、`ALIYUN_OSS_ACCESS_KEY_SECRET`：OSS 上传凭据。

发布顺序为：

1. macOS Apple Silicon、Linux x86_64、Windows x86_64 分别构建所选插件；带 sidecar 的插件同时生成平台签名，纯 WASM 插件不生成签名清单。
2. 三个平台产物上传为短期 Actions Artifact，签名临时文件立即删除。
3. Linux 汇总任务只合并所选插件，校验其 ID、版本、清单与 WASM 完全一致；带 sidecar 的插件必须具备三个平台的 sidecar 和签名清单，纯 WASM 插件的 sidecar 为空且不要求签名清单。
4. 发布任务使用所有插件共享的串行锁，读取 OSS 当前总目录并仅替换本次插件条目；读取失败时直接停止，不允许用空目录继续发布。
5. 先上传 `plugins/<id>/<version>/` 不可变制品及 `plugins-index/releases/<id>.json` 独立入口。
6. 合并后的完整目录按客户端结构、版本、地址和校验值规则验证通过后，保存上一份目录，再更新 `plugins-index/catalog.json`。
7. 从 OSS 回读正式目录并再次验证结构和 SHA-256；失败时立即恢复上一份目录并核对恢复结果。

任何平台构建、单插件目录合并、旧目录下载或制品上传失败都不会执行最后的总目录更新，因此客户端不会发现半发布版本。完整目录包含任何无效插件条目时也会在上传前终止。不同插件可以同时构建，但必须按上述批次限制进入发布阶段；发布阶段串行读取和更新总目录，不会互相覆盖。合并逻辑可在本地对一个插件准备好的平台目录执行：

```bash
TIANGONG_PLUGIN_EXPECTED_PLATFORMS=darwin-aarch64,linux-x86_64,windows-x86_64 \
  cargo run -p xtask -- merge-plugin-dist memory target/plugin-platforms target/plugin-release
```

将该插件独立发布目录合入现有总目录：

```bash
cargo run -p xtask -- merge-plugin-catalog \
  current-catalog.json \
  target/plugin-release/plugins-index/catalog.json \
  catalog.merged.json
```

单独验证一个完整目录：

```bash
cargo run -p xtask -- validate-plugin-catalog catalog.json
```

## v2 插件形态（Plugin Harness）：纯 UI 插件与界面挂载

> 关联设计：`docs/plugin-harness-design.md`；SDK：`plugins/sdk/README.md`。

`schema_version: 2` 在 v1（WASM 逻辑层）之上新增两类能力：

1. **纯 UI 插件**：`wasm` 字段可省略——只要 HTML/CSS/JS 就能做出现在拓展区矩阵的
   App（无工具/生命周期等逻辑能力，经宿主桥接 `storage.*` 持久化数据）。
2. **界面挂载声明**：`ui.contributions` 把界面挂到宿主 Slot（拓展区标签页、设置页
   等），不再局限于设置页。

### 先区分虚拟 DOM 与渲染容器

两者可以同时使用，不是互斥的插件模式：

| 概念 | 决定内容 | 选择方式 |
| --- | --- | --- |
| Vue/React 虚拟 DOM | 插件内部如何根据状态更新界面 | 插件自己的前端技术选型 |
| Shadow DOM | 插件如何嵌入 App，样式隔离但共享 JavaScript 环境 | `sandbox: "shadow"`（默认） |
| iframe | 插件运行在独立文档，脚本隔离更强 | `sandbox: "iframe"` |

官方轻量 UI 插件推荐 **Vue 3 虚拟 DOM + Shadow DOM 容器**。需要加载不可信页面
或要求脚本强隔离时使用 iframe；是否使用 Vue 不影响容器选择。

### 最小零构建示例

```sh
# 1. 生成骨架（仓库根目录执行）
cargo run -p xtask -- new-plugin com.example.myboard

# 2. 编辑 dist/plugins/com.example.myboard/app/index.html 实现界面

# 3. 天工「设置 → 插件管理 → 导入本地插件」选择该目录
#    拓展区矩阵中出现「看板」，打开即用
```

脚手架适合直接编写 HTML/CSS/JS。需要响应式状态、组件或工程化构建时，再按下文
改为 Vue 工程。两种写法使用同一份 schema v2 清单。

### Vue 3 + Vite 工程结构

纯 UI 工程可参考 `plugins/tiangong-plugin-interaction`；带 WASM/sidecar 的混合插件可参考
`plugins/screenshot-input`。Vue 工程的界面部分结构如下：

```text
my-plugin/
├── plugin.json
├── package.json
├── yarn.lock
├── index.html
├── src/
│   ├── main.ts       # 读取 Shadow 运行时、挂载和卸载 Vue
│   └── App.vue       # 虚拟 DOM 界面与 scoped 样式
├── scripts/
│   └── package.mjs   # 组装 release/
├── tsconfig.json
└── vite.config.ts    # Vue + 单文件内联构建
```

清单将 UI 声明为 Shadow 贡献，并指向构建产物：

```jsonc
{
  "schema_version": 2,
  "id": "com.example.myboard",
  "version": "0.1.0",
  "entrypoints": ["desktop"],
  "permissions": ["storage.private"],
  "ui": {
    "sandbox": "shadow",
    "contributions": [
      { "slot": "extension.tab", "id": "board", "title": "看板",
        "entry": "dist/index.html", "open_mode": "multi" }
    ]
  }
}
```

字段约束：`slot` 必须是宿主登记的合法挂载点（非法值导入即拒）；
`open_mode` 仅对 `extension.tab` 生效（`singleton` 聚焦已有 / `multi` 每次新建，
缺省 `singleton`）；`sandbox` 缺省 `shadow`（挂载主 DOM 树），`iframe` 强隔离，
`native` 仅官方签名。

Vue 入口必须从 SDK 取得实际插件根节点。Shadow 模式不能使用
`document.querySelector('#app')` 代替 `runtime.root`，否则会误查宿主页面：

```ts
import { createApp } from 'vue';
import { getShadowHostRuntime } from '@tiangong/plugin-sdk';
import App from './App.vue';

const runtime = getShadowHostRuntime();
const target = (runtime?.root ?? document).querySelector('#app');
if (!(target instanceof HTMLElement)) throw new Error('缺少 #app 挂载节点');

Object.assign(target.style, {
  boxSizing: 'border-box',
  width: '100%',
  height: '100%',
  margin: '0',
});

const app = createApp(App);
app.mount(target);
runtime?.registerCleanup(() => app.unmount());
```

`registerCleanup` 不可省略。插件更新、禁用、卸载或 Slot 销毁时，宿主会调用登记的
清理函数；事件订阅和定时器也应在 Vue 卸载阶段一并释放。

### 实例资源与生命周期（`instance_resources`）

App 实例若持有页面之外的后端资源（PTY、子进程、远程会话、webview 页面等），在
`extension.tab` 贡献上声明 `"instance_resources": true`，由宿主统一编排生命周期。
原则：**拓展区标签是资源的唯一所有者**，归属分两层——作用域
`(plugin_id, session_id)` + 实例编号 `instance_id`（即标签编号，宿主生成 scru128）。

```json
{ "slot": "extension.tab", "id": "console", "title": "控制台",
  "entry": "dist/index.html", "open_mode": "multi", "instance_resources": true }
```

- 只允许用于 `extension.tab`；非 webview 沙箱必须声明 `sidecar`（资源方）。
- 资源一律按 `instance_id` 归属；页面从 `runtime.context.app.instance_id` 取得编号。
- sidecar 必须实现两个宿主协议操作（幂等）：
  - `instanceClosed {session_id, instance_id}` → 释放该实例资源，未知编号返回成功；
  - `listInstances {session_id}` → `{instances: [{instance_id}]}`，列出会话下仍持有的实例。
- 后台（工具）创建资源的统一流程：预留编号 → 经 `host_action` 进度帧发
  `app.open {session_id, instance_id, mode: "background"}` → 有上限地等待页面附着
  （`protocol::FRONTEND_ATTACH_WAIT_MS`，超时照常执行）→ 使用。
- 关闭唯一路径：`registerBeforeClose` 只做状态保存 → 宿主移除标签后保证发出
  `instanceClosed`。不要在 `beforeClose` 里做唯一一次释放。
- 会话切换：离开的会话由宿主隐藏实例；切回时宿主按 `listInstances` 以同一编号恢复
  标签，并核查——资源方持有但没有标签的实例会被直接释放（记 warn 日志）。
- 逻辑删除会话：宿主释放该会话下全部实例。

无后端资源的 App 不要声明该字段，只用 `registerCleanup` 释放页面内资源即可。

### 样式与主题

Vue 组件统一使用 `<style scoped>`。Shadow 会隔离插件选择器，而 App 根节点的 CSS
变量、字体和 `color-scheme` 会自然继承进 Shadow，不需要订阅主题或复制颜色：

```vue
<style scoped>
.panel {
  border: 1px solid hsl(var(--border, 214.3 31.8% 91.4%));
  border-radius: var(--radius, 0.5rem);
  background: hsl(var(--background, 0 0% 100%));
  color: hsl(var(--foreground, 222.2 47.4% 11.2%));
}
</style>
```

注意事项：

- 颜色 token 保存的是 HSL 通道，使用时写成 `hsl(var(--foreground))`，不要把通道值
  当成完整 CSS 颜色。
- `scoped` 只约束组件样式，不能设置 Shadow 宿主尺寸；固定尺寸的 Slot 应在
  `main.ts` 设置 `#app`，外层尺寸由宿主 Slot 决定。
- Shadow 入口解析后只注入 `head`/`body` 的子节点，不要依赖插件样式中的
  `html`、`body`、`:root` 选择器。iframe 兼容路径可在 `main.ts` 中只对自己的文档
  做页面重置。
- `onContextChange` 用于会话等非样式信息。若脚本必须判断当前明暗主题，可读取 App
  根节点 class 或计算后的 CSS 变量；纯样式适配不应建立主题订阅。

### 宿主桥接

插件 UI 内经统一桥接访问宿主（两种容器自动适配，见 `plugins/sdk`）：

```ts
import {
  createTiangongBridge,
  openExtensionApp,
  pluginStorage,
} from '@tiangong/plugin-sdk';
const bridge = await createTiangongBridge();
await pluginStorage.set(bridge, 'tasks', JSON.stringify(list));

// 仅 extension.tab App 可用；false 表示建立可见标签但不自动展开拓展区。
await openExtensionApp(bridge, { sessionId, showPanel: false });
```

- `storage.get/set/delete/list`：私有数据，落盘在插件 `data/` 目录，需声明
  `storage.private` 权限。
- `plugin.*`：转发到本插件 WASM 逻辑层（带逻辑层的 v2 插件与 v1 插件通用）。
- `session.*`、`tool.*` 等宿主能力按清单权限开放，负载使用 JSON 字符串。
- `openExtensionApp`：声明 `extension.tab` 且具有 `app.use` 权限的 App 插件打开自身实例；`showPanel` 由插件决定是否自动展开拓展区。
- iframe 通过 `tiangong_host_context` 接收主题 token；Shadow 直接继承 App 根变量。

不要在插件中调用 Tauri API 或宿主内部函数；所有跨边界行为都应走 SDK 桥接，宿主
会按 `plugin.json` 的权限校验。

### 构建、打包与本地导入

Vite 使用 `vite-plugin-singlefile` 内联脚本和样式：

```ts
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';
import { viteSingleFile } from 'vite-plugin-singlefile';

export default defineConfig({
  base: './',
  plugins: [vue(), viteSingleFile()],
});
```

开发循环：

```sh
yarn install
yarn typecheck
yarn package
```

纯 UI 插件的 `yarn package` 应先构建，再组装仅包含 `plugin.json` 与 `dist/` 的
`release/`，并校验清单入口存在。带 sidecar 的混合插件还必须包含 WASM、当前平台
sidecar、`release.json` 与官方签名；具体脚本参考
`plugins/screenshot-input/scripts/package.mjs`。不要导入源码目录，也不要把
`node_modules`、`dist` 或 `release` 提交到 Git。

最后在天工「设置 → 插件管理 → 导入本地插件」选择 `release/`。修改已安装插件时
同步提升 `plugin.json` 和 `package.json` 的版本，再重新打包导入。

### 带逻辑层的 v2 插件

在 v1 清单基础上补 `schema_version: 2` 与 `ui`/`capabilities` 即可：WASM 侧
（工具/提示词/生命周期）零改动，界面从「仅设置页」升级为可挂拓展区矩阵。

## 交互处理器插件（审批与用户征询）

Desktop 可安装纯 TypeScript 工具插件，不需要 WASM，也不修改 `plugin.wit`。
插件在 manifest 的 `tools` 与 `prompt` 中声明工具规格和提示词，使用
`tool.provide` 权限接收 `tool.requested`，再通过 `tool.resolve` 提交完整工具结果。

默认交互处理器插件见 **`plugins/tiangong-plugin-interaction`**（Vue 3 + Vite 工程：
`plugin.json` 声明 `request_user`，`src/App.vue` 完成六类参数解析、界面、
15 秒倒计时和 answered/expired/cancelled 结果生成。宿主只转发不透明工具调用，
并保留会话归属和 20 秒通用兜底时限，不解释审批结果。审批结果返回 Agent 后，
由 Agent 自行决定后续步骤。

SDK 的通用封装是 `createToolProvider`（onRequested/onClosed/resolve），见
`plugins/sdk`。同一机制可供其他 Desktop TS 工具使用，运行时不识别
`request_user` 或任何征询类型。

## Subagent 消息卡片格式

这是**Subagent 特定消息的显示约定**，不是所有插件消息的通用格式。宿主界面
（`frontend/src/components/message/UserMessageGroup.tsx`）按消息开头的固定
文字格式识别并把消息渲染成卡片；不匹配这些格式的消息仍按普通用户消息气泡
展示，不会被加工。

**这些格式只决定消息如何显示，不能用来证明消息来源或授予权限。** 任何插件
都可以拼出同样文字，宿主不做来源校验。

### 支持的消息格式

四种格式都必须出现在**消息开头**（前缀之后不允许有其他文字），标点需要精确
匹配（全角冒号 `：`、全角括号 `【】`、半角方括号 `[]`、成员名与 `Subagent`
之间的间隔符为中点 `·` U+00B7）：

```text
【Subagent 任务】来自主会话：
任务正文

【Subagent 消息】来自成员「审查员」：
消息正文

【Subagent 回报】来自成员「审查员」：
回报正文

[Subagent·审查员] 任务完成：回报正文
```

- 前两种渲染为任务指派/协作消息卡片，「来自」后的来源原样显示在卡片头部。
- `【Subagent 回报】` 渲染为回报卡片，来源形如 `成员「某某」` 时剥去包装、
  头部显示成员名。
- `[Subagent·成员名]` 是 Hook 回报路径：状态词后跟正文，冒号与空白可省略。

### 支持的状态

Hook 回报支持四种状态，按正文开头的状态词识别：`任务完成`、`执行失败`、
`运行阻塞`、`等待审批`。没有匹配状态的 Hook 回报按普通「消息」样式展示。

### 任务附加信息

任务/消息卡片的正文按以下标记分段，各段渲染为独立的折叠行（默认收起）：

```text
任务正文

【任务工作区】
/workspace/path

【长期指令】
…

【长期记忆】
…

【成长约定】
…

【工作区状态】
…
```

第一个标记之前的内容是核心正文；每段内容延伸到下一个标记或正文末尾。正文
末尾形如 `（运行标记…）` 的片段会被识别为运行标记，从展示中剥离。

### 兼容与限制

- 推荐使用 `【任务工作区】` 新格式；旧的 `任务工作区：路径` 仅作为「任务
  工作区」一段的兼容写法保留，其余四种标记没有旧格式。
- 分段标记在正文中**任意位置**都会被识别（不要求行首），因此正文中应避免
  把这些标记当作普通文字使用，否则会被拆成折叠区。
- 前缀、空格与全角/半角标点须与上述格式完全一致，任何偏差都会导致卡片
  不生效、退回普通气泡展示。

### 显示行为

- 核心正文超过五行时折叠，可展开查看全部。
- 附加信息默认折叠，收起状态下预览开头少量文字。
- 卡片上的复制按钮始终复制**完整原始消息**（含前缀与全部标记）。
