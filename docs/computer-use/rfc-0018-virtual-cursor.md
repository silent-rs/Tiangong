# RFC 0018：天工虚拟指针（Agent 桌面操作可视化）

- 状态：已实施（Phase 1，macOS）
- 关联：RFC 0017（主动截图与图片注入）、`docs/computer-use/exception-analysis.md`
- 归属：`plugins/tiangong-plugin-computer-use`（实现完全位于插件内）

## 1. 动机

`desktop_action` 走 AX 语义动作（`AXPress` / `AXSetValue` 等），**系统鼠标指针纹丝不动**。Agent 连续操作桌面应用时，用户看不到任何指针活动，既无法感知「AI 正在操作哪里」，也无法在演示/审查场景跟随 Agent 的动作。需要一个虚拟指针把操作落点可视化。

## 2. 方案

### 2.1 指针形态

白底箭头 + 靛紫渐变描边（#6366f1 → #a855f7）+ 柔光阴影；右下角跟随「天工」胶囊徽标（深底白字、紫描边），明确标识这是天工 Agent 的指针而非系统鼠标。

- 真源：`resources/virtual-cursor.svg`（128×96 画布；hotspot 即箭头尖端位于 (16,12)）
- sidecar 嵌入图：`resources/virtual-cursor.png`（512×384 = 4x，RGBA 透明底），由 `resources/gen_cursor_png.swift` 按 SVG 同规格一次性生成后入库
- **复用约定**：后续嵌入式浏览器等需要同一指针时，直接引用 `resources/virtual-cursor.svg`（webview 场景矢量最佳）；两份产物改动时需保持规格一致（坐标/颜色/徽标）

### 2.2 实现位置：插件 sidecar 内自建 overlay 窗口

不新增宿主窗口、不扩展 core/协议——虚拟鼠标整体是 computer-use 插件的自身能力。

**按需分身（轮次生命周期）**：指针不再由 Agent 显式开关（`virtual_cursor` 工具已
移除）。本轮首次鼠标手势（`desktop_input` 的鼠标动作）时，指针从系统鼠标当前位置
（`NSEvent.mouseLocation`）以「分身」形式出现，平滑滑向目标（≈60fps 逐帧指数趋近，
自带减速缓动）；之后本轮的鼠标手势与 AX 语义动作（`desktop_ui` perform）都驱动它
跟随落点。轮次结束时插件 `on_turn_finished` 钩子通知 sidecar 收起（淡出）；没有鼠标
操作的轮次指针不出现，AX 语义动作也不会单独召唤它。钩子未送达（取消、异常）时
overlay 空闲 120 秒自动收起兜底。

```
desktop_input(click, x, y)         → overlay::glide_and_wait（未显示则从系统鼠标位置分身）
                                   → 系统鼠标闪移借用完成点击 → 归还
desktop_ui(perform, element)       → overlay::follow_to(控件中心)（仅已显示时跟随）
on_turn_finished（wasm 钩子）       → virtual_cursor 操作 enabled=false → 淡出
```

**线程模型**：AppKit 断言 NSApplication/NSWindow 只能在进程主线程使用（子线程触发
Objective-C 异常直接 abort）。sidecar `main` 把服务循环交给工作线程，主线程专跑
overlay 事件循环（手动泵 + 16ms 帧）；`move_to`/`set_enabled` 经全局 channel 非阻塞
投递，可在任意线程调用；服务退出经 `request_shutdown` 归还主线程。

overlay 窗口属性（`sidecar/src/backend/overlay.rs`，macOS；Windows 实现见下文平台对照表）：

- `NSStatusWindowLevel`（25）置顶；borderless、透明背景、无阴影
- `ignoresMouseEvents = true`：点击完全穿透，不干扰用户与目标应用
- `ActivationPolicy::Prohibited` + `orderFrontRegardless`：无 Dock 图标、永不激活、不抢焦点
- 首个 `show_at` 懒启动专用 AppKit 线程（手动泵事件循环驱动绘制与淡出）；命令经 channel 非阻塞投递，失败静默——指针是纯增益可视化，不得拖垮桌面动作

坐标换算：AX 全局坐标（主屏左上原点）→ AppKit 全局坐标（主屏左下原点），`window_origin(x, y, screen_top)` 纯函数（含单测）；bounds 零/负尺寸（读取失败默认值）不显示指针。

### 2.3 desktop_mouse：坐标级鼠标手势（CGEvent 合成）

`desktop_action`（AX 语义动作）唤不出右键菜单、没有滚轮、拖不了拽，Canvas 等自绘
UI 还没有 AX 树——坐标级合成输入是其补充。工具暴露的是**手势而非裸事件**：
`down`/`up` 永不单独出现，`drag` 由实现侧生成插值轨迹（ease-out 步进）保证拖拽
会话时序，杜绝跨工具调用的按住状态泄漏（cliclick/Playwright 同款设计）。

```
desktop_mouse { gesture: move | click | right_click | double_click | drag | scroll,
                x, y, to_x?, to_y?, delta_y?, delta_x? }
```

**系统鼠标「借用并归还」**（用户系统鼠标不被劫持）：

- `move`：纯虚拟——只动天工指针，不投递任何系统事件
- `click/right_click/double_click`：真实 HID down/up 序列（窗口激活、上下文菜单、
  菜单跟踪与真人完全一致——定向 `CGEventPostToPid` 实测无法唤出依赖 WindowServer
  状态的菜单），借用后立即把系统鼠标移回原位（≈100ms 闪回）
- `drag`：真实指针轨迹（拖拽会话必须跟随系统指针），结束归还原位
- `scroll`：无激活/菜单依赖，AX 定位坐标处进程后定向投递，系统鼠标不动

虚拟指针全程独立平滑跟随手势目标，点击瞬间播放按压脉冲动画（0→1.25→1 正弦
回弹，hotspot 始终对准目标点）。合成事件需要辅助功能授权（与 AX 同一 TCC）。

### 2.4 desktop_keyboard：键盘合成输入（CGEvent）
补齐合成输入族的键盘半边（微信场景「能看、能点、不能打字」的最后缺口）。
键盘事件投递给**当前焦点应用**（与真人一致），不触碰系统鼠标，无借用归还
问题；输入焦点由 Agent 先用 `desktop_mouse click`（或 AX focus）建立。
同样以手势而非裸事件暴露：

```
desktop_keyboard { action: type | key | combo, text?, key?, keys? }
```

- `type`：逐字符 Unicode 字符串分派（`CGEventKeyboardSetUnicodeString`，
  down/up 成对携带同一字符串）——不经输入法组字、不依赖物理布局，
  中文/表情与 ASCII 同路径（微信 Qt 输入框的关键能力）
- `key`：单键名 → 虚拟键码（HIToolbox ANSI 表）；别名不敏感
  （enter→return、esc→escape、backspace→delete、del→forward_delete…）
- `combo`：修饰键 down（保持给定顺序）→ 普通键 down/up → 修饰键逆序 up 的
  真实 HID 序列（部分应用监听修饰键本身，与点击手势同款真实管线）；
  要求恰好一个非修饰键（cmd+c、cmd+shift+3），单独按修饰键没有语义
- 键名集合：修饰键 cmd/rcmd/ctrl/rctrl/alt/ralt/shift/rshift + 非修饰键
  （return/tab/space/delete/forward_delete/escape/方向键/home/end/page_up/
  page_down/help/f1–f12/a–z/0–9，US ANSI 布局）；`fn` 不可合成，明确拒绝
- wasm 侧只做结构校验（type→text、key→key、combo→keys≥2），键名合法性由
  sidecar `keyboard::perform` 单点裁决，避免两份键码表漂移

## 3. 复用路线（嵌入式浏览器）

浏览器插件的自动化（web_click 等）发生在天工窗口内的 webview，无系统鼠标参与，同样有落点可视化需求。预留：

- 指针图片直接引用本插件的 `resources/virtual-cursor.svg`（webview 内 `<img>`/内联矢量渲染）
- 浏览器侧实现归浏览器插件自身（DOM 指针元素 + 页面坐标换算），不在本 RFC 范围

## 4. 平台与限制

### 4.1 统一模型（2026-09-24 定稿）

**虚拟指针 + 闪移借用是平台无关的产品模型**，两职责：

1. **可视化**：虚拟指针平滑滑行到落点，让用户知道 agent 在操作什么；
2. **不与用户鼠标冲突**：绝不平滑挪动用户的系统鼠标——点击绕不开系统鼠标（各平台按钮事件均作用于指针当前位置），因此以**最短占用**方式借用：闪移到目标 → down/up → 闪移归还（≈100ms，用户无感，鼠标位置不丢）。

序列编排平台无关：虚拟指针滑行**并等待真正到达**（`glide_and_wait` 到位回执）→ 停顿 → 闪移借用点击 → 归还。drag 轨迹例外：拖拽会话必须由系统指针承载，借用期间系统指针承载轨迹、虚拟指针跟随。

| 平台 | 视觉主角（overlay） | 闪移借用实现 | 状态 |
|---|---|---|---|
| macOS | NSStatusWindow 置顶窗 | down/up 事件**自带绝对坐标**，无需瞬移指针（借用最优雅的特例） | ✅ 实装 |
| Windows | 分层置顶穿透窗（`WS_EX_LAYERED \| WS_EX_TRANSPARENT \| WS_EX_NOACTIVATE`，`UpdateLayeredWindow` 逐像素 alpha，`WDA_EXCLUDEFROMCAPTURE` 不进截图；专用 overlay 线程泵消息） | `SetCursorPos` → `SendInput` down/up → `SetCursorPos` 归还；滚轮同样借用后归还 | ✅ 实装（`win_input.rs` / `win_overlay.rs` / `win_keycast.rs`） |
| Linux X11 | override-redirect 窗 | `XTestFakeMotionEvent` → `XTestFakeButtonEvent` → 归还 | 待实装 |
| Wayland | — | 合成输入被安全模型禁止 | 维持 AT-SPI 语义动作 |

实装建议：手势序列编排（插值节奏、到位等待、先移动后点击、归还）抽平台无关层，平台层仅提供 `post_motion(x,y)` / `post_button(down/up)` / overlay 三组原语。

Windows 补充：sidecar 启动时声明 Per-Monitor-V2 DPI 感知，UIA bounds、鼠标坐标与截图 `logical_bounds` 统一为虚拟桌面物理像素（`scale` 语义不变）；键盘 `type` 用 `KEYEVENTF_UNICODE` 逐 UTF-16 单元分派，`cmd` 等同 Ctrl，徽标键为 `win`，按键 HUD 显示 `Ctrl/Alt/Shift/Win` 文字键帽；截图为 GDI `BitBlt(CAPTUREBLT)` + WIC JPEG；`open` 先匹配运行中窗口（进程名/标题）恢复并置前，未运行时查开始菜单快捷方式（显示名/本地化名）再交给 `ShellExecuteEx`。UIPI 限制：无法向以管理员身份运行的窗口注入输入（`SendInput` 明确报错）。

- 限制：macOS 仅主屏坐标正确（多屏待后续）；定向投递的滚轮在个别自绘应用可能不生效；Linux 键鼠工具返回不支持（清晰失败，agent 自动转语义动作路径）
