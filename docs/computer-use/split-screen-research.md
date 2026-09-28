# Computer Use 自动分屏：调研与实现

- 分支：`feature/computer-use-split-screen`（基于 `origin/main` 5aeb5159）
- 目标：执行 computer use 打开应用时，天工窗口固定在屏幕左侧、宽 400；被操作的应用铺满右侧剩余区域，方便同时看会话和操作过程。
- 状态：macOS / Windows 已实现（第 4 节），Linux 不在范围内。验证情况见第 6 节。

## 1. 结论速览

| 平台 | 系统原生分屏（Split View / Snap） | 能否精确放置「左 400 + 右剩余」 | 推荐实现 | 权限/前置 |
| --- | --- | --- | --- | --- |
| macOS | Split View 与 Sequoia 窗口平铺都**没有公开 API**，且只支持 1/2、1/4 等固定比例 | 可以，用 AX 直接设窗口 frame | `AXUIElementSetAttributeValue(kAXPosition/kAXSize)` | 辅助功能授权（插件已要求） |
| Windows | Snap Layouts / Snap Assist **没有公开 API**，模拟 Win+←/→ 只能对半分 | 可以，用 Win32 直接设窗口位置 | `SetWindowPos` + 显示器工作区 `rcWork` | 目标进程不能是管理员权限（UIPI） |
| Linux X11 | 各窗口管理器不统一 | 可以（EWMH） | `_NET_MOVERESIZE_WINDOW` / `XMoveResizeWindow` | 无需额外权限 |
| Linux Wayland | 协议层面**禁止客户端摆放其他应用的窗口** | 通用方案做不到 | 只能按合成器适配（Sway/Hyprland IPC、KWin 脚本），其余情况降级 | 取决于合成器 |

**核心判断**：各平台系统自带分屏都只有固定比例、并且不开放调用接口，满足不了「左侧 400px」这个要求。应该绕开系统分屏，**由我们自己计算两块区域，再分别设置两个窗口的位置和大小**（下文称「自定义平铺」）。macOS、Windows、Linux X11 都能完整实现；Wayland 只能尽力适配，做不到时降级。

## 2. 现有代码基础（本仓库）

computer-use 插件在 `plugins/tiangong-plugin-computer-use/`，sidecar 为独立 Rust 进程。

- 已有能力：列出窗口、控件树、截图、打开应用、键鼠合成、虚拟指针/按键 HUD。**还没有任何移动或缩放窗口的能力**，protocol 里也没有对应的操作（现有操作见 `protocol/src/ops.rs` 里的 `DESKTOP_*_OPERATION`）。
- macOS
  - `sidecar/src/backend/ax.rs:90` 已声明 `AXUIElementSetAttributeValue`，并封装了 string/bool 两种设置函数（`ax.rs:330`、`ax.rs:350`）。还需要补 `AXValueCreate`（CGPoint/CGSize）的封装。
  - `sidecar/src/backend/macos.rs:1216` 的 `window_frame_for_app` 能用 CGWindowList 读出窗口 frame（左上角为原点的全局 points，和 AX 坐标系一致），可以用来在设置后读回核对。
  - `app_launch.rs:453` 已用过 `AXRaise`。
  - 已依赖 `objc2-app-kit` 的 `NSScreen`，可以直接拿 `visibleFrame`，不用加新依赖。
- Windows
  - `sidecar/src/backend/win_desktop.rs:130` 的 `window_bounds` 已用 `DWMWA_EXTENDED_FRAME_BOUNDS` 读取可见边框。
  - `win_desktop.rs:286` 的 `find_app_window` 和 `win_desktop.rs:576` 的 `activate_window` 可以直接复用。
  - `windows` crate 已启用 `Win32_UI_WindowsAndMessaging`、`Win32_Graphics_Dwm`、`Win32_UI_HiDpi`，`SetWindowPos` 和 `MonitorFromWindow/GetMonitorInfoW` 都能用，不用加新依赖。
- Linux：`backend/linux.rs` 只接了 AT-SPI（zbus），明确不依赖 xdotool/wmctrl。
- 天工主窗口：`src-tauri/tauri.conf.json` 里 `minWidth: 400`、`titleBarStyle: Overlay`。400px 刚好等于最小宽度，可以缩到这个宽度，但**前端在 400px 下的布局要单独检查**（侧边栏、标签页、输入框）。
- 宿主 PID：sidecar 启动时会拿到环境变量 `TIANGONG_PLUGIN_HOST_PID`（`crates/tiangong-plugin-runtime/src/sidecar/stdio/mod.rs:56`）。sidecar 可以据此找到天工窗口，用同一套 AX/Win32 逻辑摆放它，不需要额外打通 Tauri 命令。
  - 需要确认：这个 PID 在打包版和开发版里是不是都是持有主窗口的进程。

## 3. 各平台细节

### 3.1 macOS

**系统原生分屏**

- 全屏 Split View：只能靠用户按住绿色按钮或在 Mission Control 里操作。没有公开 API，AppleScript/System Events 也不能可靠触发，而且需要两个窗口都进入全屏 Space。不采用。
- macOS 15 Sequoia 的窗口平铺（菜单「窗口 > 移动与调整大小」、拖到屏幕边缘、⌃🌐+方向键）：只有半屏、四分之一等固定布局。可以用菜单栏的 AX 动作或快捷键触发，但定制不了 400px。不采用。
- 台前调度（Stage Manager）：开启时会影响窗口位置，并在左侧占一条缩略图带，需要检测并提示用户。

**自定义平铺（推荐）**

1. 取屏幕：用 `NSScreen` 找天工窗口所在的屏幕，取 `visibleFrame`（已扣掉菜单栏和 Dock）。注意 AppKit 的原点在左下角，要换算成 AX 的左上角全局坐标（这一步 `keycast.rs` 和 `overlay.rs` 里已经有过）。
2. 取窗口：`AXUIElementCreateApplication(pid)`，取 `kAXFocusedWindowAttribute` 或 `kAXMainWindowAttribute`。
3. 设置：先 `kAXPositionAttribute`，再 `kAXSizeAttribute`，最后再设一次 position。部分应用在缩放后会自己挪位置，所以要补这一次。
4. 核对：读回 AX frame 或 CGWindowList frame，确认是否达到目标。

**需要注意**

- 全屏窗口：先把 `AXFullScreen` 设为 false 并等退出全屏的动画结束，否则设置 frame 不生效。
- 最小/固定尺寸：有些应用有最小宽度或根本不能缩放。先用 `AXUIElementIsAttributeSettable(kAXSize)` 判断；设置后读回，达不到目标时如实返回实际 frame。
- 多显示器：两个窗口统一放在天工当前所在的屏幕。
- 权限：设置其他进程的窗口需要辅助功能授权。插件已经要求这个授权，不新增权限。
- 天工自己的窗口：可以用 AX 按宿主 PID 设置，也可以在 Tauri 进程里调 `WebviewWindow::set_position/set_size`（逻辑坐标）。建议 sidecar 统一用 AX，跨平台逻辑一致，也不用改宿主。

### 3.2 Windows

**系统原生分屏**

- Snap Layouts / Snap Assist / FancyZones：没有公开的编程接口。模拟 `Win+←` 只能半屏，还会弹出 Snap Assist 选择界面，干扰操作。不采用。

**自定义平铺（推荐）**

1. 取工作区：`MonitorFromWindow(hwnd_tiangong, MONITOR_DEFAULTTONEAREST)`，再 `GetMonitorInfoW` 取 `rcWork`（已扣掉任务栏）。
2. 还原窗口：最大化或最小化的窗口先 `ShowWindow(SW_RESTORE)`，否则 `SetWindowPos` 的结果会被系统覆盖。
3. 设置：`SetWindowPos(hwnd, HWND_TOP, x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER …)`，天工窗口和目标窗口各设一次。
4. 扣掉隐形边框：Win10/11 的窗口有约 7px 的透明缩放边框。先对比 `GetWindowRect` 和 `DWMWA_EXTENDED_FRAME_BOUNDS` 算出差值再补偿，否则两个窗口之间会有缝隙或重叠。

**需要注意**

- DPI：sidecar 已经是 Per-Monitor DPI 感知，拿到的是物理像素。「400px」应该按逻辑像素处理，实际宽度 = `400 × GetDpiForMonitor 缩放比例`。
- UIPI：非管理员进程不能移动管理员权限进程的窗口，`SetWindowPos` 会静默失败。需要读回核对，并提示用户。
- UWP/商店应用：真正的窗口是 `ApplicationFrameHost` 的顶层窗口，`find_app_window` 已按顶层窗口枚举，一般没问题，需要实测。
- 最小尺寸：应用可能通过 `WM_GETMINMAXINFO` 限制最小尺寸，同样需要读回核对。

### 3.3 Linux

**X11**

- 标准做法：给根窗口发 EWMH 的 `_NET_MOVERESIZE_WINDOW` 客户端消息（也就是 wmctrl 的做法）；如果窗口是最大化状态，先通过 `_NET_WM_STATE` 去掉 `_NET_WM_STATE_MAXIMIZED_HORZ/VERT`。
- 工作区用 `_NET_WORKAREA`，多显示器时结合 XRandR。
- 需要新增依赖，比如 `x11rb`（纯 Rust）。也可以试 AT-SPI `Component::SetExtents`，它在部分 X11 工具包上有效，但不可靠，只适合作兜底。

**Wayland**

- `xdg-shell` 协议不允许客户端决定窗口在全局的位置，更不允许摆放其他应用的窗口。这是设计上的限制，没有通用办法。
- 只能按合成器单独适配：
  - Sway/i3 兼容：`swaymsg` IPC，可以 `move position` / `resize set`。
  - Hyprland：`hyprctl dispatch movewindowpixel/resizewindowpixel`，或 IPC socket。
  - KDE KWin：通过 D-Bus 加载 KWin 脚本来设置 `frameGeometry`。
  - GNOME Mutter：从 GNOME 41 起默认关闭 `org.gnome.Shell.Eval`，只能靠用户装的扩展。基本做不到。
- 建议：Wayland 下先检测 `XDG_SESSION_TYPE` / `XDG_CURRENT_DESKTOP`，第一期只返回「不支持自动分屏」和原因，不影响 computer use 其他能力。以后再按需适配 Sway/Hyprland/KWin。

## 4. 最终方案（已按用户决定实现）

用户决定（2026-09-28）：
- 不新增 `desktop_arrange` 工具；**打开应用（`desktop_app` action=open）成功后自动分屏**。
- 输入框发送按钮左侧的按钮区（`session.input-action` 挂载点）增加「恢复窗口」按钮，由用户主动恢复天工窗口；**其他应用不再调整**。
- 按钮只在「当前会话用过自动分屏」且「对话已完成（轮次结束）」时显示；点击恢复后隐藏；该会话新一轮开始时也先隐藏。
- Linux 的 computer use 支持不完整，**不做自动分屏**（后端默认返回不支持）。
- 多显示器：两个窗口都放在天工窗口当前所在的屏幕。

实现位置（`plugins/tiangong-plugin-computer-use/`）：

| 模块 | 职责 |
| --- | --- |
| `protocol/src/ops.rs` | `OpenAppResponse.split`、`SplitOutcome`、`SplitState`、`split_layout`（纯函数）及 `split_state` / `split_turn` / `restore_host_window` 三个操作 |
| `sidecar/src/split.rs` | 分屏状态：天工原位置（只记第一次）、分屏发起会话、可显示按钮的会话；状态变化经 `emit_notification("computer_use.split")` 推送 |
| `sidecar/src/backend/mac_split.rs` | macOS：`AXPosition/AXSize` 摆放；先退出全屏；屏幕可用区域由 overlay 主线程代查 `NSScreen.visibleFrame` |
| `sidecar/src/backend/overlay.rs` | 新增 `ScreenWorkArea` 命令（NSScreen 只能在主线程访问） |
| `sidecar/src/backend/win_split.rs` | Windows：`SetWindowPos` + `rcWork` + DWM 可见边框补偿 + 按 DPI 换算 400 |
| `wasm/src/lib.rs` | open 成功且分屏生效时标记本轮；`on_turn_started/finished` 通知 sidecar；`handle_view_message` 提供 `splitState` / `restoreHostWindow` |
| `app/restore-window.html` | 按钮 UI（shadow 容器）：拉取状态 + 订阅 `sidecar.event`，隐藏时收起宿主容器 |
| `plugin.json` | 新增 `bridge.call` 权限、`capabilities.events: ["sidecar.*"]`、`session.input-action` 贡献；版本 0.4.0 |

天工窗口定位：sidecar 由天工宿主直接拉起（官方 computer-use 不进 OS 沙箱），环境变量 `TIANGONG_PLUGIN_HOST_PID` 即主窗口所在进程；本机实测天工主窗口进程与该 PID 一致。

## 5. 已知限制

- 窗口不可缩放（如计算器）时，只移动到右侧区域左上角、保持原尺寸，summary 说明原因；有最小尺寸时按实际读回位置返回。
- Windows 目标以管理员身份运行时 `SetWindowPos` 会失败（UIPI），summary 说明原因。
- 分屏状态保存在 sidecar 进程内存中：天工或 sidecar 重启后按钮不再出现，窗口保持当前位置。
- 天工前端在 400px 宽度下的布局需要单独检查。

## 6. 验证

- 自动化：`split_layout` 布局计算、`OpenAppResponse` 兼容性、分屏状态机（按钮显示/隐藏规则）、容差判定均有单元测试。
- Windows：本机无法交叉编译完整 sidecar（依赖 aws-lc-sys 的 C 构建），`win_split.rs` 在隔离工程中对 `x86_64-pc-windows-msvc` 通过 clippy；真机行为（边框补偿、150% DPI、管理员窗口）需在 Windows 上验证。
- macOS 真机：`sidecar/examples/split_demo.rs` 用天工 PID 作宿主、目标 PID 作目标，执行分屏、读回、恢复：

  ```sh
  TIANGONG_PLUGIN_HOST_PID=<天工 pid> cargo run -p tiangong-plugin-computer-use-sidecar \
    --example split_demo -- <目标 pid> [停留秒数]
  ```

  需要运行它的终端具有辅助功能权限（macOS 27 改名为「隐私与安全 → 设备控制和数据访问」）；天工沙箱内的终端拿不到该权限。

  实测结果（macOS 27.0，1512×982 屏幕，天工 pid 作宿主，2026-09-28）：

  | 目标 | 天工窗口 | 目标窗口 | 恢复 |
  | --- | --- | --- | --- |
  | 文本编辑（可缩放） | (0,33) 400×859 | (400,33) 1112×858 | 回到 (6,33) 1400×859 ✓ |
  | 计算器（固定尺寸） | (0,33) 400×859 | (400,33) 230×408，保持原尺寸 | 回到原位 ✓ |

  首次实测发现计算器 `AXSize` 不可写时整体报「不允许移动」，已改为只移动位置。

## 附：调研阶段的草案（已废弃）

最初草案是新增独立工具 `computer_use.desktop_arrange`，并在任务结束后恢复两个窗口；用户决定改为「打开应用自动分屏 + 只恢复天工窗口的按钮」，Linux 不在范围内。

## 参考

- Apple AXUIElement：<https://developer.apple.com/documentation/applicationservices/axuielement_h>
- Apple NSScreen.visibleFrame：<https://developer.apple.com/documentation/appkit/nsscreen/visibleframe>
- Microsoft SetWindowPos：<https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setwindowpos>
- Microsoft GetMonitorInfoW：<https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getmonitorinfow>
- Microsoft DwmGetWindowAttribute：<https://learn.microsoft.com/windows/win32/api/dwmapi/nf-dwmapi-dwmgetwindowattribute>
- freedesktop EWMH 规范：<https://specifications.freedesktop.org/wm-spec/latest/>
- Wayland xdg-shell：<https://wayland.app/protocols/xdg-shell>
