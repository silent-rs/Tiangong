//! macOS Seatbelt profile 编译器。
//!
//! 双语义适配：macOS 26（darwin 25）起未提及的操作类别不再默认放行，
//! 且通配读放行会覆盖定点禁读；旧系统保持 allow-by-default。语义按
//! 运行时实测判定（版本号不可靠）：新系统显式放行基础能力并以细分读
//! 类别保护敏感路径，旧系统保持既有的通配规则。

use std::fmt::Write as _;
use std::path::Path;

use super::policy::SandboxPolicy;

const SEATBELT_BIN: &str = "/usr/bin/sandbox-exec";

pub fn seatbelt_available() -> bool {
    seatbelt_probe().is_ok()
}

/// 探测用的最小可执行 profile：macOS 26（darwin 25）起 Seatbelt 未提及
/// 类别不再默认放行，探测必须显式放行 exec 与读，否则探测自身被拒。
const PROBE_PROFILE: &str = "(version 1)(allow process-exec*)(allow file-read*)";

/// 一次性真实探测：宿主环境若已在 Seatbelt 沙箱内（嵌套终端/受限 CI 外壳），
/// sandbox-exec 无法再次应用沙箱。失败时携带探测输出，供错误报告定位
/// 受限来源（而不是只给一个结论）。
pub fn seatbelt_probe() -> Result<(), String> {
    static PROBE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    PROBE
        .get_or_init(|| {
            if !Path::new(SEATBELT_BIN).exists() {
                return Err("未找到 /usr/bin/sandbox-exec".to_string());
            }
            match std::process::Command::new(SEATBELT_BIN)
                .arg("-p")
                .arg(PROBE_PROFILE)
                .arg("/usr/bin/true")
                .output()
            {
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    let detail = if !stderr.is_empty() {
                        stderr
                    } else if !stdout.is_empty() {
                        stdout
                    } else {
                        format!("退出状态 {}", out.status)
                    };
                    Err(format!(
                        "探测命令 sandbox-exec -p '{PROBE_PROFILE}' /usr/bin/true 失败：{detail}"
                    ))
                }
                Err(error) => Err(format!("无法执行 sandbox-exec 探测：{error}")),
            }
        })
        .clone()
}

/// macOS 26（darwin 25）起未提及的操作类别不再默认放行，必须显式列出
/// sidecar 所需的基础能力，并避免通配读规则覆盖定点禁读。
///
/// 按内核版本判断不可靠：GitHub macos-15 runner（darwin 24）实测同样
/// 未提及即拒（execvp EPERM，退出码 71），按版本走 legacy 分支导致 CI
/// 全部真实执行测试失败。改为运行时实测语义——不放行 exec 的读放行
/// profile 若能执行，说明未提及类别默认放行（旧语义），反之新语义。
fn requires_explicit_categories() -> bool {
    static SEMANTICS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SEMANTICS.get_or_init(|| {
        std::process::Command::new(SEATBELT_BIN)
            .arg("-p")
            .arg("(version 1)(allow file-read*)")
            .arg("/usr/bin/true")
            .output()
            .is_ok_and(|out| !out.status.success())
    })
}

/// 编译为 SBPL profile 文本。
pub fn compile_profile(policy: &SandboxPolicy) -> String {
    let writable = if policy.mode == super::policy::SandboxMode::WorkspaceWrite {
        policy.writable_roots()
    } else {
        Vec::new()
    };
    // SBPL 注入防护：路径会拼进 profile 文本，控制字符或非 UTF-8 内容可能
    // 破坏规则结构。不安全路径编译为拒绝一切的 profile（fail-closed），
    // 绝不放行可能被注入的规则。
    let paths_safe = writable
        .iter()
        .chain(policy.read_only_roots().iter())
        .chain(policy.denied_read_roots().iter())
        .all(|path| sbpl_safe_path(path));
    if !paths_safe {
        return "(version 1)\n(deny default)\n".to_string();
    }

    if requires_explicit_categories() {
        compile_profile_explicit_categories(policy, &writable)
    } else {
        compile_profile_legacy(policy, &writable)
    }
}

/// darwin >= 25（macOS 26 Seatbelt 语义）的实测规则（darwin 25.5 逐项验证）：
/// - 通配读放行（allow file-read*）会短路定点禁读——读侧必须用细分类别
///   （file-read-data / file-read-metadata）放行，禁读同理细分并前置；
/// - 写侧用带路径过滤的 file-write*：工作区先放行、敏感路径随后锁死，
///   宿主显式授予的额外可写根最后覆盖；
/// - 未提及类别默认拒绝：exec/fork/sysctl-read 必须显式放行（sysctl
///   缺失时 Rust 运行时 guard page 分配直接 EINVAL 崩溃）。
fn compile_profile_explicit_categories(
    policy: &SandboxPolicy,
    writable: &[std::path::PathBuf],
) -> String {
    let mut sbpl = String::new();
    sbpl.push_str("(version 1)\n");
    for path in policy.denied_read_roots() {
        let escaped = escape(&path);
        let _ = writeln!(sbpl, "(deny file-read-data (literal \"{escaped}\"))");
        let _ = writeln!(sbpl, "(deny file-read-data (subpath \"{escaped}\"))");
        let _ = writeln!(sbpl, "(deny file-read-metadata (literal \"{escaped}\"))");
        let _ = writeln!(sbpl, "(deny file-read-metadata (subpath \"{escaped}\"))");
    }
    sbpl.push_str("(allow file-write* (literal \"/dev/null\"))\n");
    for root in writable {
        let _ = writeln!(sbpl, "(allow file-write* (subpath \"{}\"))", escape(root));
    }
    // 路径过滤规则按后出现者覆盖同类放行：先锁死工作区内的敏感路径。
    for path in policy.read_only_roots() {
        let escaped = escape(&path);
        let _ = writeln!(sbpl, "(deny file-write* (literal \"{escaped}\"))");
        let _ = writeln!(sbpl, "(deny file-write* (subpath \"{escaped}\"))");
    }
    // 防篡改与敏感清单必须最终覆盖一切可写授权（含 extra_writable——
    // writable 参数已并入）：extra 段曾后置于此，祖先级可写根（存储根）
    // 会覆盖工作区内的定点 deny，实测确认新语义为后出现者覆盖。
    sbpl.push_str("(deny file-write*)\n");
    // 全局读放行必须细分：通配 file-read* 会短路上面的定点禁读。
    sbpl.push_str("(allow file-read-data)\n(allow file-read-metadata)\n");
    if policy.allow_network {
        sbpl.push_str("(allow network*)\n");
    } else {
        sbpl.push_str("(deny network*)\n");
    }
    append_system_service_rules(&mut sbpl, policy);
    sbpl.push_str("(allow process-exec*)\n(allow process-fork)\n");
    // 宿主退出时 sidecar 必须能终止自己创建的整棵进程树；target children
    // 只含直接子进程。按继承的沙箱实例授权，覆盖孙进程但不包括宿主或
    // 独立启动的其他沙箱（即使使用相同策略）。
    sbpl.push_str("(allow signal (target same-sandbox))\n");
    // Rust/C 运行时启动需读 sysctl（页大小——guard page 计算），zsh 5.9
    // 同样读 hw.* sysctl（Tahoe 上多方踩坑）；不显式放行直接崩。
    sbpl.push_str("(allow sysctl-read)\n");
    // PTY 三件套（对齐系统 application.sb 的官方写法）：终端插件 openpty
    // 需要 pseudo-tty 类别、主设备 /dev/ptmx（读写+ioctl）与从设备
    // /dev/tty* 的读写与 ioctl 放行。portable-pty 在从设备上执行
    // TIOCSCTTY；缺少从设备 ioctl 时子进程 spawn 返回 EPERM。
    // 带 literal/regex 过滤器的 allow 只作用于匹配路径，不影响定点禁读。
    sbpl.push_str("(allow pseudo-tty)\n");
    sbpl.push_str("(allow file-read* file-write* file-ioctl (literal \"/dev/ptmx\"))\n");
    sbpl.push_str("(allow file-read* file-write* file-ioctl (regex \"^/dev/tty[^\\\\.]\"))\n");
    sbpl
}

/// darwin < 25（后匹配生效 + 未提及默认放行）：保持既有规则顺序——
/// 全局放行/拒绝在前，定点规则在后靠覆盖生效（Codex 验证过的顺序）。
fn compile_profile_legacy(policy: &SandboxPolicy, writable: &[std::path::PathBuf]) -> String {
    let mut sbpl = String::new();
    sbpl.push_str("(version 1)\n");
    // 读全盘放行（工具链需要读系统目录、SDK、home 缓存）。
    sbpl.push_str("(allow file-read*)\n");
    // 默认禁写，再逐个放行；/dev/null 是大量脚本的基础依赖。
    sbpl.push_str("(deny file-write*)\n");
    sbpl.push_str("(allow file-write* (literal \"/dev/null\"))\n");
    for root in writable {
        let _ = writeln!(sbpl, "(allow file-write* (subpath \"{}\"))", escape(root));
    }
    // 防篡改段：写在 allow 之后才能重新锁死（后规则覆盖先规则）。
    for path in policy.read_only_roots() {
        let escaped = escape(&path);
        let _ = writeln!(sbpl, "(deny file-write* (literal \"{escaped}\"))");
        let _ = writeln!(sbpl, "(deny file-write* (subpath \"{escaped}\"))");
    }
    // 敏感路径读取规则位于全局 file-read 放行之后，精确路径与子路径都拒绝。
    for path in policy.denied_read_roots() {
        let escaped = escape(&path);
        let _ = writeln!(sbpl, "(deny file-read* (literal \"{escaped}\"))");
        let _ = writeln!(sbpl, "(deny file-read* (subpath \"{escaped}\"))");
    }
    if !policy.allow_network {
        sbpl.push_str("(deny network*)\n");
    }
    append_system_service_rules(&mut sbpl, policy);
    sbpl
}
/// 证书验证随网络授权开放；Keychain 与用户身份解析只随凭据授权开放；
/// CoreAudio 输入只随音频授权开放。
/// 两组均限制到精确服务名，不允许任意系统服务查询。
fn append_system_service_rules(sbpl: &mut String, policy: &SandboxPolicy) {
    if policy.allow_network {
        // 与系统 system.sb 的证书验证服务一致，覆盖系统及当前用户的 trustd。
        sbpl.push_str(
            "(allow mach-lookup (global-name \"com.apple.trustd\"))\n\
             (allow mach-lookup (global-name \"com.apple.trustd.agent\"))\n",
        );
    }
    if policy.allow_credential_services {
        sbpl.push_str(
            "(allow mach-lookup (global-name \"com.apple.SecurityServer\"))\n\
             (allow mach-lookup (global-name \"com.apple.securityd.xpc\"))\n\
             (allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n\
             (allow mach-lookup (global-name \"com.apple.system.opendirectoryd.membership\"))\n",
        );
    }
    if policy.allow_audio_input {
        // 实测 cpal/CoreAudio 采集默认输入设备的最小集合：缺 audiohald 无法
        // 枚举设备，缺 AudioComponentRegistrar 找不到 HAL 输入 AudioUnit；
        // 缺 device-microphone 时设备能打开、回调照常触发，但 coreaudiod 对
        // 沙箱进程做 sandbox_check 后只下发静音（不报错，采样全 0）。
        sbpl.push_str(
            "(allow mach-lookup (global-name \"com.apple.audio.audiohald\"))\n\
             (allow mach-lookup (global-name \"com.apple.audio.AudioComponentRegistrar\"))\n\
             (allow device-microphone)\n",
        );
    }
    // 显式拒绝名单之外的服务，也约束未提及类别默认放行的旧系统。
    sbpl.push_str("(deny mach-lookup)\n");
}

/// 路径可安全进入 SBPL 文本：必须是 UTF-8 且不含控制字符
/// （换行/NUL 等会破坏 profile 结构）。括号在字符串字面量内无特殊含义，
/// 不拒绝以免误伤 macOS 常见文件名。
fn sbpl_safe_path(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|text| !text.chars().any(char::is_control))
}

/// 构造 sandbox-exec 的 argv 前缀（不含被包装命令本身）。
pub fn wrap_argv(policy: &SandboxPolicy) -> Vec<String> {
    vec!["-p".to_string(), compile_profile(policy)]
}

fn escape(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn audio_input_services_follow_explicit_authorization() {
        for audio in [false, true] {
            let mut policy = SandboxPolicy::workspace_write("/tmp/ws");
            policy.allow_audio_input = audio;
            for sbpl in [
                compile_profile_explicit_categories(&policy, &policy.writable_roots()),
                compile_profile_legacy(&policy, &policy.writable_roots()),
            ] {
                assert!(sbpl.contains("(deny mach-lookup)"));
                for service in [
                    "com.apple.audio.audiohald",
                    "com.apple.audio.AudioComponentRegistrar",
                ] {
                    let rule = format!("(allow mach-lookup (global-name \"{service}\"))");
                    assert_eq!(
                        sbpl.contains(&rule),
                        audio,
                        "音频服务 {service} 必须独立授权"
                    );
                    if audio {
                        // 放行必须位于兜底拒绝之前才生效。
                        assert!(sbpl.find(&rule) < sbpl.find("(deny mach-lookup)"));
                    }
                }
                // 麦克风数据同样只随音频授权放行（缺失时只采到静音）。
                assert_eq!(
                    sbpl.contains("(allow device-microphone)"),
                    audio,
                    "device-microphone 必须独立授权"
                );
            }
        }
    }

    #[test]
    fn credential_services_rules_follow_explicit_authorization() {
        for network in [false, true] {
            for credentials in [false, true] {
                let mut policy = SandboxPolicy::workspace_write("/tmp/ws");
                policy.allow_network = network;
                policy.allow_credential_services = credentials;
                // 两种编译路径都必须保留精确授权边界。
                for sbpl in [
                    compile_profile_explicit_categories(&policy, &policy.writable_roots()),
                    compile_profile_legacy(&policy, &policy.writable_roots()),
                ] {
                    assert!(!sbpl.contains("(allow mach-lookup)"));
                    assert!(!sbpl.contains("mach-lookup*"));
                    assert!(sbpl.contains("(deny mach-lookup)"));
                    for service in ["com.apple.trustd", "com.apple.trustd.agent"] {
                        assert_eq!(
                            sbpl.contains(&format!(
                                "(allow mach-lookup (global-name \"{service}\"))"
                            )),
                            network,
                            "证书服务 {service} 必须只随网络授权开放"
                        );
                    }
                    for service in [
                        "com.apple.SecurityServer",
                        "com.apple.securityd.xpc",
                        "com.apple.system.opendirectoryd.libinfo",
                        "com.apple.system.opendirectoryd.membership",
                    ] {
                        assert_eq!(
                            sbpl.contains(&format!(
                                "(allow mach-lookup (global-name \"{service}\"))"
                            )),
                            credentials,
                            "凭据服务 {service} 必须独立授权"
                        );
                    }
                    assert_eq!(sbpl.contains("(deny network*)"), !network);
                }
            }
        }
    }

    #[test]
    fn profile_denies_write_outside_roots_and_network() {
        let mut policy = SandboxPolicy::workspace_write("/tmp/ws");
        policy.protected_paths = vec!["/tmp/ws/protected".into()];
        policy.denied_read_paths = vec!["/tmp/home/.ssh".into()];
        let sbpl = compile_profile(&policy);
        assert!(sbpl.contains("(deny file-write*)"));
        assert!(sbpl.contains("(deny network*)"));
        // 防篡改段与工作区放行的相对顺序和类别形式随系统语义而定：
        // 新系统（先匹配）用细分类别且 deny 前置，旧系统用通配且 deny
        // 靠覆盖在后。
        let ws_allow = sbpl
            .find("(allow file-write* (subpath \"/tmp/ws\"))")
            .unwrap();
        if requires_explicit_categories() {
            let protected_deny = sbpl
                .find("(deny file-write* (subpath \"/tmp/ws/protected\"))")
                .unwrap();
            assert!(
                protected_deny > ws_allow,
                "显式类别语义下防篡改 deny 必须覆盖工作区放行"
            );
            // 读侧为避免通放短路定点禁读，全局放行必须是细分类别。
            assert!(sbpl.contains("(allow file-read-data)\n(allow file-read-metadata)\n"));
            assert!(sbpl.contains("(deny file-read-data (subpath \"/tmp/home/.ssh\"))"));
            // Rust 运行时的基础依赖必须显式放行（未提及即拒绝）。
            assert!(sbpl.contains("(allow sysctl-read)"));
            assert!(sbpl.contains("(allow file-read* file-write* file-ioctl (regex \"^/dev/tty"));
        } else {
            let protected_deny = sbpl
                .find("(deny file-write* (subpath \"/tmp/ws/protected\"))")
                .unwrap();
            assert!(protected_deny > ws_allow);
            assert!(sbpl.contains("(allow file-read*)"));
            assert!(sbpl.contains("(deny file-read* (subpath \"/tmp/home/.ssh\"))"));
        }
    }

    #[test]
    fn readonly_mode_has_no_writable_roots() {
        let mut policy = SandboxPolicy::workspace_write("/tmp/ws");
        policy.mode = crate::sandbox::policy::SandboxMode::ReadOnly;
        let sbpl = compile_profile(&policy);
        // .git 防篡改已无条件声明（deny 会含 /tmp/ws/.git），只断言
        // 工作区不再出现在可写放行里。
        assert!(!sbpl.contains("allow file-write* (subpath \"/tmp/ws\")"));
        assert!(sbpl.contains("(deny file-write*)"));
    }

    #[test]
    fn wrap_argv_contains_only_launcher_arguments() {
        let policy = SandboxPolicy::workspace_write("/tmp/ws");
        let argv = wrap_argv(&policy);
        assert_eq!(argv[0], "-p");
        assert!(argv[1].contains("(version 1)"));
        assert!(!argv.iter().any(|arg| arg == SEATBELT_BIN));
    }

    #[test]
    fn generated_profile_is_accepted_by_sandbox_exec() {
        if seatbelt_probe().is_err() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let profile = compile_profile(&SandboxPolicy::workspace_write(workspace.path()));
        let output = std::process::Command::new(SEATBELT_BIN)
            .arg("-p")
            .arg(profile)
            .arg("/usr/bin/true")
            .output()
            .expect("执行 sandbox-exec 策略解析检查失败");
        assert!(
            output.status.success(),
            "生成的 Seatbelt 策略无法解析：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn control_character_path_compiles_to_deny_all() {
        // 换行注入尝试：不安全路径必须编译为拒绝一切的 profile，
        // 而不是让注入内容进入规则文本。
        let policy = SandboxPolicy::workspace_write("/tmp/ws\n(deny file-write*)");
        let sbpl = compile_profile(&policy);
        assert_eq!(sbpl, "(version 1)\n(deny default)\n");

        // 括号在字符串字面量内合法（macOS 常见文件名），不触发拒绝。
        let policy = SandboxPolicy::workspace_write("/tmp/demo(1)");
        let sbpl = compile_profile(&policy);
        assert!(sbpl.contains("(allow file-write* (subpath \"/tmp/demo(1)\"))"));
    }
}
