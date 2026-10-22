# Fs Plugin

Fs 插件对模型暴露 3 个工具：

- `file_read`：只读，`action` 取 `list`（列目录）/ `tree`（目录树）/ `read`（读文本文件）/ `image`（读图片并注入对话）；
- `file_write`：写入，`action` 取 `write`（写入/追加）/ `replace`（文本替换）/ `patch`（应用 unified diff）；
- `current_time`：当前本地时间（含时区偏移）。

工具图标在 plugin.json `tool_icons` 中声明为自带 SVG：通用键 `file_read` 为读文件图标，特指键 `file_read:action=list` / `file_read:action=tree` 为目录图标、`file_read:action=image` 为图片图标；`file_write`、`current_time` 各一个图标。

工具调用的展开区由工具视图 `app/tool-view.html` 渲染（`session.message-item` replace 贡献，`tools` 声明接管 `file_read` / `file_write` / `current_time`）：按 action 显示读取内容（带行号）、写入内容、替换前后与补丁 diff、读图结果（来源、尺寸、格式、大小与媒体副本），以及结果或错误。工具行标题由同一贡献的 `titles` 按 action 声明（如 `file_read:action=list` 为「列出目录」），宿主显示，视图本身不重复标题。

## 结构

插件没有 WASM 层，工具规格写在 plugin.json `tools` 中，由宿主直连 sidecar 执行：

- `protocol`：工具名、action 取值与文件操作的请求/响应（含 `FsAccessContext` 访问能力上下文，为后续沙箱能力预留）；
- `sidecar`：参数解析、文件读写（`std::fs`）、写操作锁、路径解析与读图规范化。生命周期为按需（每次调用独立进程），工作目录取自宿主注入的请求上下文（会话 cwd）；写操作的加锁/解锁经进度通道上报 `file_lock_changed` 事件。

完整插件使用以下命令统一检查、构建和部署：

```bash
cargo run -p xtask -- build-plugin fs
```

## 沙箱预留

- **路径策略可替换**：路径解析集中在 sidecar；未来引入路径白名单 / overlay 时只换实现，业务代码不动。
- **访问能力随请求下发**：`FsAccessContext`（protocol 层）承载当前会话的 `workspace`（及预留的 `full_trust`），未来细化权限只扩该结构。
