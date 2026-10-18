# Workspace Files 插件

在拓展区以「文件」标签浏览、预览和编辑当前会话工作区内的文件。

## 能力

- 文件树：按需展开目录，目录在前、名称不区分大小写排序；单目录最多返回 5000 项。
- 预览：UTF-8 文本（≤ 2 MB）可直接编辑；常见图片（≤ 10 MB）以内嵌方式预览；其余显示为二进制。
- 编辑：保存采用内容摘要（sha256）乐观锁，文件在外部被修改后拒绝覆盖并提示重新加载；
  写入走同目录临时文件 + rename，保留原文件权限。`Cmd/Ctrl+S` 保存。
- 管理：新建文件/文件夹、重命名、删除（二次确认后直接删除，不进回收站）。

## 安全边界

- 只接受工作区相对路径；绝对路径、`..` 与经符号链接逃逸到工作区外的目标一律拒绝。
- sidecar 为按需生命周期（每次调用独立进程），宿主以会话权威工作区作为 OS 沙箱可写域；
  页面传入的工作区仅用于定位根目录，越界写入同时被沙箱拦截。宿主调用携带
  调用上下文时以上下文中的工作区为准。

## 结构

```
plugin.json          清单（schema v2，extension.tab + 按需 sidecar）
assets/icon.svg      拓展区 App 图标（插件自带，经 resources 声明随包分发）
sidecar/             Rust sidecar（files.rs 文件操作，service.rs 请求分发）
src/                 Vue 页面（api.ts 桥接封装，App.vue 文件树/预览/编辑）
```

## 开发

```bash
yarn install && yarn build                                 # 页面产物 dist/index.html
cargo test -p tiangong-plugin-workspace-files-sidecar      # sidecar 单元测试
cargo run -p xtask -- build-plugin workspace-files          # 打包插件
```
