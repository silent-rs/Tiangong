# T002 - Manifest schema v2 解析与校验

## 目标

在现有 `plugin.json` 解析基础上支持 `schema_version: 2`，解析并校验新增的 `capabilities`、`ui.contributions`（含 `open_mode`、`sandbox`、`context`）字段，旧 v1 清单继续按旧规则解析。

## 范围

- 后端：`crates/plugin/tiangong-plugin-runtime/src/manifest.rs`（或对应清单解析模块）扩展 schema v2 字段解析与校验。
- 相关：`registry.rs` 中导入时的清单校验调用链。

## 依赖

- 前置任务：T001（贡献类型、Slot Registry 已就绪）。
- 后续任务：T004（前端容器读取贡献）、T007（extension.tab 元数据）。
- 可并行任务：T003（桥接命令层）。
- 阻塞说明：v2 清单解析出的 `UiContribution` 要落到 T001 的类型上，并复用 Slot Registry 校验合法 Slot。

## 任务

- [ ] `schema_version: 2` 的 `capabilities` 字段解析（`tools`/`prompt`/`lifecycle`/`approval`/`interaction`/`events`）。
- [ ] `ui.contributions` 数组解析，字段：`slot`、`id`、`title`、`icon`、`entry`、`open_mode`、`context`、`sandbox`。
- [ ] 校验规则：`slot` 必须是 Slot Registry 登记的合法 ID；`open_mode` 仅对 `extension.tab` 生效、缺省 `singleton`；`sandbox` 缺省 `shadow`，`native` 需官方签名。
- [ ] 兼容规则：`schema_version: 1` 按旧规则解析，`ui` 缺省等价于「仅设置页」，映射到 `settings.plugin-page`。
- [ ] 错误信息可读：非法 Slot、非法 `open_mode`、未知字段给出明确报错并拒绝导入。

## 不做

- 不实现 `native` 容器的运行时渲染（T006/T011 之后）。
- 不实现桥接命令（T003）。
- 不改动签名验证流程（沿用现有 sidecar/release 签名校验）。

## 验收

- v2 清单解析出完整 `UiContribution`，字段与 T001 类型一致。
- 非法 Slot / 非法 `open_mode` 被拒绝并给出可读错误。
- v1 清单解析结果等价旧行为，回归不破坏。

## 验证

- `cargo check -p tiangong-plugin-runtime`
- 补充 manifest 解析单元测试：v2 正常解析、v2 非法 Slot 拒绝、v1 兼容解析、`open_mode` 缺省值。
- `cargo test -p tiangong-plugin-runtime`（若已存在 manifest 测试）。
