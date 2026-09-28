//! `desktop_open_app`（macOS）：唤起已运行应用，或经系统应用索引定位后启动。
//!
//! 流程：
//! 1. 按 bundle_id / 应用名匹配运行中的应用（常规应用优先，排除辅助进程）；
//! 2. 未匹配时经 Spotlight 元数据（`mdfind`）与应用目录扫描定位 `.app`，
//!    名称匹配覆盖文件名、`CFBundleName`/`CFBundleDisplayName` 与本地化
//!    `InfoPlist.strings`（「微信」↔ WeChat.app）；
//! 3. 经 LaunchServices（`open -b/-a`）启动或发送 reopen——与点击程序坞
//!    等价，窗口被关闭时也会重新显示主窗口；
//! 4. 取消隐藏、激活（系统切换到窗口所在空间）、AX 取消最小化并置前；
//! 5. 轮询 CGWindowList 直到出现屏幕窗口，返回其逻辑坐标。
//!
//! 所有 objc 对象只在同步辅助函数内使用，不跨 await 持有（非 Send）。
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplicationActivationOptions, NSApplicationActivationPolicy, NSWorkspace};
use objc2_foundation::{NSDictionary, NSString};
use tiangong_plugin_computer_use_protocol::ops::{OpenAppRequest, OpenAppResponse};
use tiangong_plugin_computer_use_protocol::{Bounds, DesktopError, DesktopResult};

use super::ax::{self, AxElement};

/// 启动后等待进程出现的上限。
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
/// 激活后等待屏幕窗口出现的上限。
const WINDOW_TIMEOUT: Duration = Duration::from_secs(6);
/// `mdfind` 查询上限（Spotlight 被禁用时立即返回空）。
const MDFIND_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 运行中应用的纯数据视图（不持有 objc 对象）。
#[derive(Debug, Clone)]
struct RunningApp {
    pid: i32,
    name: String,
    bundle_id: Option<String>,
    regular: bool,
}

/// 磁盘上定位到的应用包。
#[derive(Debug, Clone)]
struct BundleInfo {
    path: PathBuf,
    bundle_id: Option<String>,
    name: String,
}

fn running_apps() -> Vec<RunningApp> {
    let workspace = NSWorkspace::sharedWorkspace();
    workspace
        .runningApplications()
        .iter()
        .map(|app| RunningApp {
            pid: app.processIdentifier(),
            name: app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            bundle_id: app.bundleIdentifier().map(|s| s.to_string()),
            regular: app.activationPolicy() == NSApplicationActivationPolicy::Regular,
        })
        .collect()
}

/// 名称匹配得分：0 = 完全相同（大小写不敏感），1 = 前缀，2 = 包含；None 不匹配。
fn name_score(candidate: &str, needle: &str) -> Option<u8> {
    let c = candidate.trim().to_lowercase();
    let n = needle.trim().to_lowercase();
    if c.is_empty() || n.is_empty() {
        return None;
    }
    if c == n {
        Some(0)
    } else if c.starts_with(&n) {
        Some(1)
    } else if n.chars().count() >= 2 && c.contains(&n) {
        Some(2)
    } else {
        None
    }
}

/// 在运行中应用里挑选目标：bundle_id 精确匹配优先；名称按得分排序，
/// 常规应用（有程序坞图标）优先于辅助/代理进程。
fn select_running(
    apps: &[RunningApp],
    name: Option<&str>,
    bundle_id: Option<&str>,
) -> Vec<RunningApp> {
    let mut scored: Vec<(u8, bool, &RunningApp)> = apps
        .iter()
        .filter_map(|app| {
            if let Some(id) = bundle_id
                && app
                    .bundle_id
                    .as_deref()
                    .is_some_and(|have| have.eq_ignore_ascii_case(id))
            {
                return Some((0, !app.regular, app));
            }
            let needle = name?;
            let score = name_score(&app.name, needle)?;
            Some((score + 1, !app.regular, app))
        })
        .collect();
    // 有常规应用候选时剔除辅助进程（如「自动填充 (微信)」）。
    if scored.iter().any(|(_, helper, _)| !helper) {
        scored.retain(|(_, helper, _)| !helper);
    }
    scored.sort_by_key(|(score, helper, app)| (*score, *helper, app.pid));
    scored.into_iter().map(|(_, _, app)| app.clone()).collect()
}

/// 读取 plist / strings 文件中的字符串键（二进制、XML、OpenStep 格式均可）。
fn plist_strings(path: &Path, keys: &[&str]) -> Vec<String> {
    let Some(path_str) = path.to_str() else {
        return Vec::new();
    };
    if !path.is_file() {
        return Vec::new();
    }
    let ns_path = NSString::from_str(path_str);
    // SAFETY: 只读解析本地文件；失败返回 None，不抛异常。
    #[allow(deprecated)]
    let dict: Option<objc2::rc::Retained<NSDictionary<NSString, AnyObject>>> =
        unsafe { NSDictionary::dictionaryWithContentsOfFile(&ns_path) };
    let Some(dict) = dict else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|key| {
            let value = dict.objectForKey(&NSString::from_str(key))?;
            let text = value.downcast::<NSString>().ok()?.to_string();
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

/// 读取 `InfoPlist.loctable` 中文/英文条目的名称（简体中文优先）。
fn loctable_names(path: &Path) -> Vec<String> {
    let Some(path_str) = path.to_str() else {
        return Vec::new();
    };
    if !path.is_file() {
        return Vec::new();
    }
    let ns_path = NSString::from_str(path_str);
    // SAFETY: 只读解析本地文件；失败返回 None，不抛异常。
    #[allow(deprecated)]
    let table: Option<objc2::rc::Retained<NSDictionary<NSString, AnyObject>>> =
        unsafe { NSDictionary::dictionaryWithContentsOfFile(&ns_path) };
    let Some(table) = table else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for locale in [
        "zh_CN", "zh-Hans", "zh_TW", "zh-Hant", "zh_HK", "en", "Base",
    ] {
        let Some(entry) = table.objectForKey(&NSString::from_str(locale)) else {
            continue;
        };
        let Ok(entry) = entry.downcast::<NSDictionary>() else {
            continue;
        };
        for key in ["CFBundleDisplayName", "CFBundleName"] {
            if let Some(value) = entry.objectForKey(&*NSString::from_str(key))
                && let Ok(text) = value.downcast::<NSString>()
            {
                let text = text.to_string().trim().to_string();
                if !text.is_empty() {
                    names.push(text);
                }
            }
        }
    }
    names
}

/// 应用包的候选名称：文件名 + Info.plist 名称；`localized` 时追加
/// 中英文本地化名称。首项为展示名（本地化优先）。
fn bundle_names(path: &Path, localized: bool) -> (Option<String>, Vec<String>) {
    let info = path.join("Contents").join("Info.plist");
    let bundle_id = plist_strings(&info, &["CFBundleIdentifier"])
        .into_iter()
        .next();
    let mut names = Vec::new();
    if localized {
        let resources = path.join("Contents").join("Resources");
        // 系统应用（macOS 13+）的本地化名集中在 InfoPlist.loctable：
        // { 语言代码: { CFBundleDisplayName, CFBundleName } }。
        names.extend(loctable_names(&resources.join("InfoPlist.loctable")));
        if let Ok(entries) = std::fs::read_dir(&resources) {
            let mut lprojs: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.ends_with(".lproj")
                            && (n.starts_with("zh") || n.starts_with("en") || n == "Base.lproj")
                    })
                })
                .collect();
            // 简体中文优先，其次其他中文、英文。
            lprojs.sort_by_key(|p| {
                let n = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                match n {
                    "zh-Hans.lproj" | "zh_CN.lproj" => 0,
                    _ if n.starts_with("zh") => 1,
                    _ => 2,
                }
            });
            for lproj in lprojs {
                names.extend(plist_strings(
                    &lproj.join("InfoPlist.strings"),
                    &["CFBundleDisplayName", "CFBundleName"],
                ));
            }
        }
    }
    names.extend(plist_strings(
        &info,
        &["CFBundleDisplayName", "CFBundleName"],
    ));
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        names.push(stem.to_string());
    }
    names.dedup();
    (bundle_id, names)
}

/// 在候选应用包中挑选最佳匹配；返回 (得分, 信息)。
fn best_bundle(
    candidates: &[PathBuf],
    name: Option<&str>,
    bundle_id: Option<&str>,
    localized: bool,
) -> Option<(u8, BundleInfo)> {
    let mut best: Option<(u8, BundleInfo)> = None;
    for path in candidates {
        let (id, names) = bundle_names(path, localized);
        let id_match = bundle_id
            .zip(id.as_deref())
            .is_some_and(|(want, have)| have.eq_ignore_ascii_case(want));
        let score = if id_match {
            Some(0)
        } else {
            name.and_then(|needle| {
                names
                    .iter()
                    .filter_map(|n| name_score(n, needle))
                    .min()
                    .map(|s| s + 1)
            })
        };
        let Some(score) = score else { continue };
        if best.as_ref().is_none_or(|(b, _)| score < *b) {
            let display = names.first().cloned().unwrap_or_default();
            best = Some((
                score,
                BundleInfo {
                    path: path.clone(),
                    bundle_id: id,
                    name: display,
                },
            ));
            if score == 0 {
                break;
            }
        }
    }
    best
}

/// Spotlight 元数据查询（系统搜索能力）；带超时，Spotlight 禁用时为空。
fn spotlight_candidates(name: Option<&str>, bundle_id: Option<&str>) -> Vec<PathBuf> {
    // 查询串内嵌用户输入：含引号/反斜杠/通配符时跳过，避免注入查询语法。
    let safe = |s: &str| !s.chars().any(|c| matches!(c, '\'' | '"' | '\\' | '*'));
    let mut clauses = Vec::new();
    if let Some(id) = bundle_id.filter(|s| safe(s)) {
        clauses.push(format!("kMDItemCFBundleIdentifier == '{id}'c"));
    }
    if let Some(n) = name.filter(|s| safe(s)) {
        clauses.push(format!("kMDItemDisplayName == '*{n}*'cd"));
        clauses.push(format!("kMDItemFSName == '*{n}*'cd"));
    }
    if clauses.is_empty() {
        return Vec::new();
    }
    let query = format!(
        "kMDItemContentType == 'com.apple.application-bundle' && ({})",
        clauses.join(" || ")
    );
    let Ok(mut child) = Command::new("/usr/bin/mdfind")
        .arg(query)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + MDFIND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| l.ends_with(".app"))
        .map(PathBuf::from)
        .take(50)
        .collect()
}

/// 标准应用目录扫描（深度 2：`<根>/*.app` 与 `<根>/*/*.app`）。
fn directory_candidates() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        PathBuf::from("/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    let is_app = |p: &Path| p.extension().is_some_and(|e| e == "app");
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if is_app(&path) {
                out.push(path);
            } else if path.is_dir()
                && let Ok(nested) = std::fs::read_dir(&path)
            {
                out.extend(
                    nested
                        .filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| is_app(p)),
                );
            }
        }
    }
    out
}

/// 经系统搜索定位应用包：Spotlight 优先，目录扫描兜底；先比对廉价
/// 名称，无完全匹配时再读取本地化名称。
fn locate_bundle(name: Option<&str>, bundle_id: Option<&str>) -> Option<BundleInfo> {
    let spotlight = spotlight_candidates(name, bundle_id);
    let directory = directory_candidates();
    let mut best: Option<(u8, BundleInfo)> = None;
    for localized in [false, true] {
        for set in [&spotlight, &directory] {
            if let Some(found) = best_bundle(set, name, bundle_id, localized)
                && best.as_ref().is_none_or(|(b, _)| found.0 < *b)
            {
                best = Some(found);
            }
            // bundle id 或名称完全匹配即止。
            if best.as_ref().is_some_and(|(s, _)| *s <= 1) {
                return best.map(|(_, info)| info);
            }
        }
    }
    best.map(|(_, info)| info)
}

/// 经 LaunchServices 打开：未运行则启动，已运行则发送 reopen（等价点击
/// 程序坞，关闭了主窗口的应用会重新显示窗口）。bundle id 查询失败时
/// （LaunchServices 数据库未登记等）退回按应用路径打开。
fn launch_services_open(bundle_id: Option<&str>, path: Option<&Path>) -> Result<(), String> {
    let run = |flag: &str, target: &std::ffi::OsStr| -> Result<(), String> {
        let output = Command::new("/usr/bin/open")
            .arg(flag)
            .arg(target)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("启动 open 失败：{e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "open {flag} 退出码 {}：{}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    };
    match (bundle_id, path) {
        (Some(id), Some(path)) => run("-b", id.as_ref()).or_else(|first| {
            run("-a", path.as_os_str()).map_err(|second| format!("{first}；{second}"))
        }),
        (Some(id), None) => run("-b", id.as_ref()),
        (None, Some(path)) => run("-a", path.as_os_str()),
        (None, None) => Err("缺少 bundle id 与应用路径，无法经 LaunchServices 打开".to_string()),
    }
}

/// 把运行中的应用带到前台：取消隐藏、激活全部窗口、AX 置前并恢复
/// 最小化窗口。返回执行过的步骤说明。
pub(super) fn bring_to_front(pid: i32) -> Vec<String> {
    let mut steps = Vec::new();
    let workspace = NSWorkspace::sharedWorkspace();
    if let Some(app) = workspace
        .runningApplications()
        .iter()
        .find(|app| app.processIdentifier() == pid)
    {
        if app.isHidden() && app.unhide() {
            steps.push("取消隐藏".to_string());
        }
        if app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows) {
            steps.push("已激活".to_string());
        }
    }
    if ax::is_process_trusted() {
        let root = AxElement::for_application(pid);
        if root.set_bool_attribute("AXFrontmost", true).is_ok() {
            steps.push("AX 置前".to_string());
        }
        if let Ok(windows) = root.elements_attribute("AXWindows") {
            let mut restored = 0;
            for window in &windows {
                if window.bool_attribute("AXMinimized") == Some(true)
                    && window.set_bool_attribute("AXMinimized", false).is_ok()
                {
                    restored += 1;
                }
            }
            if restored > 0 {
                steps.push(format!("恢复 {restored} 个最小化窗口"));
            }
            if let Some(first) = windows.first() {
                let _ = first.perform_action("AXRaise");
            }
        }
    }
    steps
}

fn to_bounds((x, y, width, height): (f64, f64, f64, f64)) -> Bounds {
    Bounds {
        x,
        y,
        width,
        height,
    }
}

/// 执行 `desktop_open_app`。
pub(super) async fn open_app(req: &OpenAppRequest) -> DesktopResult<OpenAppResponse> {
    let name = req
        .app_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let bundle_id = req
        .bundle_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if name.is_none() && bundle_id.is_none() {
        return DesktopResult::Err(DesktopError::ApplicationNotFound {
            query: "desktop_open_app 需要 app_name 或 bundle_id".to_string(),
        });
    }
    let query = bundle_id.or(name).unwrap_or_default().to_string();

    // 1. 运行中匹配；未命中时经系统搜索定位应用包，再按其 bundle id 复查。
    let mut running = select_running(&running_apps(), name, bundle_id);
    let mut bundle: Option<BundleInfo> = None;
    if running.is_empty() {
        bundle = locate_bundle(name, bundle_id);
        if let Some(id) = bundle.as_ref().and_then(|b| b.bundle_id.as_deref()) {
            running = select_running(&running_apps(), None, Some(id));
        }
    }

    let launched = running.is_empty();
    let target_bundle_id = running
        .first()
        .and_then(|a| a.bundle_id.clone())
        .or_else(|| bundle.as_ref().and_then(|b| b.bundle_id.clone()));
    let target_path = bundle.as_ref().map(|b| b.path.clone());
    if launched && target_bundle_id.is_none() && target_path.is_none() {
        return DesktopResult::Err(DesktopError::ApplicationNotFound {
            query: format!("{query}（运行中应用与系统应用目录中均未找到）"),
        });
    }

    // 2. LaunchServices 打开 / reopen。已运行时失败不致命（仍走激活路径）。
    let open_result = launch_services_open(target_bundle_id.as_deref(), target_path.as_deref());
    if launched && let Err(reason) = &open_result {
        return DesktopResult::Err(DesktopError::BackendUnavailable {
            reason: format!("启动 {query} 失败：{reason}"),
        });
    }

    // 3. 新启动时等待进程出现。
    if launched {
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        loop {
            running = select_running(&running_apps(), name, target_bundle_id.as_deref());
            if !running.is_empty() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        if running.is_empty() {
            return DesktopResult::Err(DesktopError::Timeout {
                waited_ms: LAUNCH_TIMEOUT.as_millis() as u64,
            });
        }
    }
    let target = running[0].clone();
    // 应用包的本地化展示名（如「微信」）优先，进程名兜底。
    let display_name = bundle
        .as_ref()
        .map(|b| b.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| target.name.clone());

    // 4. 置前 + 5. 等待屏幕窗口（多进程应用的窗口可能挂在同名 helper 上，
    //    按 pid 与应用名双路匹配）。
    let mut steps = bring_to_front(target.pid);
    if let Err(reason) = open_result {
        steps.push(format!("reopen 未生效（{reason}）"));
    }
    let owner_name = (!target.name.is_empty()).then(|| target.name.clone());
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    let mut retried = false;
    let window = loop {
        let found =
            super::macos::window_frame_for_app(Some(target.pid as u32), owner_name.as_deref())
                .map(to_bounds);
        if found.is_some() || Instant::now() >= deadline {
            break found;
        }
        // 半程仍无窗口：再次置前（应用刚完成启动时首次激活可能被忽略）。
        if !retried && deadline.saturating_duration_since(Instant::now()) < WINDOW_TIMEOUT / 2 {
            retried = true;
            steps.extend(bring_to_front(target.pid));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    let action = if launched { "已启动" } else { "已唤起" };
    let summary = match &window {
        Some(b) => format!(
            "{action}「{}」（pid={}），前台窗口位于 x={:.0} y={:.0} 宽={:.0} 高={:.0}；可直接用该区域 desktop_screenshot。步骤：{}",
            display_name,
            target.pid,
            b.x,
            b.y,
            b.width,
            b.height,
            steps.join("、")
        ),
        None => format!(
            "{action}「{}」（pid={}），但 {} 秒内未检测到屏幕窗口（应用可能只有菜单栏/托盘界面，或窗口在其他桌面空间）。步骤：{}",
            display_name,
            target.pid,
            WINDOW_TIMEOUT.as_secs(),
            steps.join("、")
        ),
    };
    DesktopResult::Ok(OpenAppResponse {
        app_name: display_name,
        pid: target.pid as u32,
        bundle_id: target.bundle_id.or(target_bundle_id),
        bundle_path: target_path.map(|p| p.display().to_string()),
        launched,
        window,
        summary,
        split: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(pid: i32, name: &str, id: Option<&str>, regular: bool) -> RunningApp {
        RunningApp {
            pid,
            name: name.to_string(),
            bundle_id: id.map(str::to_string),
            regular,
        }
    }

    #[test]
    fn name_score_orders_exact_prefix_contains() {
        assert_eq!(name_score("微信", "微信"), Some(0));
        assert_eq!(name_score("WeChat", "wechat"), Some(0));
        assert_eq!(name_score("Visual Studio Code", "visual"), Some(1));
        assert_eq!(name_score("自动填充 (微信)", "微信"), Some(2));
        // 单字符不做包含匹配，避免误中。
        assert_eq!(name_score("Safari", "a"), None);
        assert_eq!(name_score("", "x"), None);
    }

    #[test]
    fn select_running_prefers_regular_exact_match() {
        let apps = vec![
            app(11372, "自动填充 (微信)", None, false),
            app(11367, "微信", Some("com.tencent.xinWeChat.helper"), false),
            app(11341, "微信", Some("com.tencent.xinWeChat"), true),
            app(500, "ChatGPT", Some("com.openai.chat"), true),
        ];
        let picked = select_running(&apps, Some("微信"), None);
        assert_eq!(picked.first().map(|a| a.pid), Some(11341));
        // 有常规应用时辅助进程被剔除。
        assert!(picked.iter().all(|a| a.regular));
        let by_id = select_running(&apps, None, Some("COM.TENCENT.XINWECHAT"));
        assert_eq!(by_id.first().map(|a| a.pid), Some(11341));
        assert!(select_running(&apps, Some("不存在"), None).is_empty());
    }

    #[test]
    fn locate_bundle_finds_system_app_by_file_name() {
        // 计算器是所有 macOS 自带应用；目录扫描兜底应能定位。
        let found = locate_bundle(Some("Calculator"), None).expect("应定位到计算器");
        assert!(found.path.ends_with("Calculator.app"), "{:?}", found.path);
        assert_eq!(found.bundle_id.as_deref(), Some("com.apple.calculator"));
    }

    #[test]
    fn locate_bundle_finds_system_app_by_localized_name() {
        // 系统应用中文名来自 InfoPlist.loctable（「计算器」↔ Calculator.app）。
        let found = locate_bundle(Some("计算器"), None).expect("应按中文名定位到计算器");
        assert!(found.path.ends_with("Calculator.app"), "{:?}", found.path);
        assert_eq!(found.name, "计算器");
    }

    #[test]
    fn locate_bundle_by_bundle_id() {
        let found = locate_bundle(None, Some("com.apple.calculator")).expect("应按 bundle id 定位");
        assert!(found.path.ends_with("Calculator.app"), "{:?}", found.path);
    }
}
