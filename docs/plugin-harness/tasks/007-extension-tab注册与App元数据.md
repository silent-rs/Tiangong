# T007 - extension.tab Slot 注册 + App 元数据

## 目标

把声明 `extension.tab` 贡献的插件暴露为「App」：后端聚合 App 元数据（插件名、贡献标题/图标/描述、打开模式、沙箱级别）并经统一命令输出，供拓展区能力矩阵（T009）与实例管理（T010）消费。

## 范围

- 后端：`crates/plugin/tiangong-plugin-runtime/src/registry.rs` 新增 App 元数据聚合；`src-tauri/src/commands.rs` 新增 `list_extension_apps` 命令。
- 前端：`frontend/src/api/tauri.ts` 补 `AppEntry` 类型与 `listExtensionApps` 封装。

## 依赖

- 前置任务：T002（manifest v2 贡献解析）、T006（沙箱容器）。
- 后续任务：T008（拓展区状态机）、T009（矩阵视图）、T010（实例管理）。
- 可并行任务：无。

## 任务

- [ ] `list_extension_apps()`：遍历已启用插件，取 manifest `ui.contributions` 中 `slot == "extension.tab"` 的贡献，聚合插件 descriptor 名称与贡献元数据（icon/title/open_mode/sandbox/description），按 (plugin_id, contribution_id) 稳定排序。
- [ ] Tauri 命令 `list_extension_apps` + 前端 `AppEntry` 类型（snake_case 对齐）与 `api.listExtensionApps` 封装。
- [ ] 单元/集成测试：v2 插件声明 extension.tab（singleton 与 multi 各一）被列为 App；非 extension.tab 贡献（如 settings.plugin-page）不进入 App 列表；禁用插件不出现。

## 不做

- 官方内置 App（浏览器/终端）的注册（T011/T012 迁移时接入，届时以官方 App 身份并入同一列表）。
- App 运行态/实例状态（T009/T010 前端实例管理时维护）。
- 前端矩阵视图与入口收敛（T008/T009）。

## 验收

- 声明 extension.tab 的 v2 插件出现在 App 列表，open_mode 语义正确（缺省 singleton、multi 显式声明）。
- 元数据含插件名（descriptor name）、贡献标题、图标、沙箱级别。
- v1 插件与禁用插件不出现在 App 列表。

## 验证

- `cargo test -p tiangong-plugin-runtime`（新增用例进 `v2_manifest_contribution.rs` 或独立集成测试）。
- `cargo check -p tiangong-app`、`yarn --cwd frontend build`。
