//! 测试支撑（`#[doc(hidden)]`：非公开 API，仅供本 crate 测试与集成测试
//! 复用）。把 debug Launcher 复制到测试进程目录（launcher_manager 的优先
//! 解析位置）并用测试密钥签名，同时把测试公钥写入 fixture 存储根作为
//! 本机用户密钥信任根——真实链路因此完整经过"启动前验签"。
//!
//! 跨进程并发安全：目标文件内容相同，写入经临时名+原子 rename。

use std::path::{Path, PathBuf};

const TEST_SIGNING_SECRET_KEY: &str = "untrusted comment: rsign encrypted secret key\nRWQAAEIyAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAls4pXFjc5mdI4GLK03g7o1W1/i77lDVt6N0UEBq1kiaRr4Tu8k4qp6+g7ZVZTZVY46bgfgMqxzLtK9cQwyK7Tjh7/3zxhx5Q4jk/sWXImXvRS/pwCH3EFfNivwZFLOJkLCbVcQ2/qz4=";
#[cfg(any(unix, windows))]
const TEST_SIGNING_PUBLIC_KEY_B64: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDY3RTZEQzU4NUMyOUNFOTYKUldTV3ppbGNXTnptWjYrZzdaVlpUWlZZNDZiZ2ZnTXF4ekx0SzljUXd5SzdUamg3LzN6eGh4NVEK";

/// workspace target/debug 下的二进制路径（不存在返回 None）。
pub fn debug_binary(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let debug_dir = executable.parent()?.parent()?;
    let candidate = debug_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// 准备测试 Launcher：复制+签名+写入信任根。Launcher 未构建时返回
/// 携带指引的 Err（调用方决定跳过或失败）。
pub fn ensure_test_launcher_signed(storage_root: &Path) -> Result<(), String> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = storage_root;
        return Err("当前平台无沙箱链路".to_string());
    }
    #[cfg(any(unix, windows))]
    {
        let launcher_source = debug_binary("tiangong-sandbox").ok_or(
            "target/debug/tiangong-sandbox 尚未构建（先 cargo build -p tiangong-sandbox）",
        )?;
        let test_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .ok_or("无法定位测试进程目录")?;
        let launcher = test_dir.join(format!("tiangong-sandbox{}", std::env::consts::EXE_SUFFIX));
        atomic_copy(&launcher_source, &launcher)?;
        sign_minisign(&launcher)?;
        let keys = storage_root.join("keys");
        std::fs::create_dir_all(&keys).map_err(|e| format!("创建信任根目录失败: {e}"))?;
        // 写入完整密钥对（私钥+公钥）：安装链的本地信任流程会调用
        // ensure_user_signing_key——私钥缺失时生成新密钥对并覆盖公钥，
        // 会顶掉测试公钥导致 Launcher 验签失败；成对写入后走"已存在，
        // 校验配对"分支，测试密钥链保持稳定。
        std::fs::write(keys.join("user-signing.key"), TEST_SIGNING_SECRET_KEY)
            .map_err(|e| format!("写入测试私钥失败: {e}"))?;
        std::fs::write(
            keys.join("user-signing.key.pub"),
            format!("{TEST_SIGNING_PUBLIC_KEY_B64}\n"),
        )
        .map_err(|e| format!("写入测试公钥失败: {e}"))?;
        // 自验：立即验证签名与信任根配对，失败信息携带实际路径，
        // 用于定位 ensure 产物与连接验签路径的分歧。
        let resolved = tiangong_sandbox_update::launcher_manager::resolve_installed_program(
            &storage_root.join("sandbox"),
        );
        crate::signature::verify_launcher_signature(
            resolved.as_ref().unwrap_or(&launcher),
            storage_root,
        )
        .map_err(|e| {
            format!(
                "测试 Launcher 自验失败（resolved={resolved:?}, launcher={launcher:?}, storage={storage_root:?}）: {e:#}"
            )
        })?;
        Ok(())
    }
}

#[cfg(any(unix, windows))]
fn atomic_copy(source: &Path, destination: &Path) -> Result<(), String> {
    let temp = destination.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::copy(source, &temp).map_err(|e| format!("复制失败: {e}"))?;
    std::fs::rename(&temp, destination).map_err(|e| format!("落位失败: {e}"))
}

#[cfg(any(unix, windows))]
fn sign_minisign(launcher: &Path) -> Result<(), String> {
    let secret_box =
        minisign::SecretKeyBox::from_string(TEST_SIGNING_SECRET_KEY).map_err(|e| e.to_string())?;
    let secret = secret_box
        .into_unencrypted_secret_key()
        .map_err(|e| e.to_string())?;
    let public = minisign::PublicKey::from_secret_key(&secret).map_err(|e| e.to_string())?;
    let data = std::fs::read(launcher).map_err(|e| e.to_string())?;
    let signature = minisign::sign(Some(&public), &secret, data.as_slice(), None, None)
        .map_err(|e| e.to_string())?;
    use base64::Engine;
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.into_string());
    let sig_path = launcher.with_file_name(format!(
        "tiangong-sandbox{}.sig",
        std::env::consts::EXE_SUFFIX
    ));
    let temp = sig_path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, sig_b64).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, &sig_path).map_err(|e| e.to_string())
}

/// 当前环境无法应用原生沙箱时的跳过原因（None 表示可用）。
///
/// 天工终端/受限 CI 等外层 Seatbelt 环境内，Launcher 无法再次嵌套应用
/// 沙箱而拒绝启动（退出码 78，安全设计而非缺陷）——真实沙箱链路测试
/// 在此类环境必然失败或卡死，统一跳过并打印原因。
///
/// 探测逻辑与 Launcher 的平台可用性判断一致（宿主不链接沙箱实现）：
/// macOS 试跑最小 Seatbelt profile，Linux 查找 bubblewrap，Windows 由
/// AppContainer 承担（视为可用）。
pub fn native_sandbox_skip_reason() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        const PROBE_PROFILE: &str = "(version 1)(allow process-exec*)(allow file-read*)";
        match std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", PROBE_PROFILE, "/usr/bin/true"])
            .output()
        {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(format!(
                "当前宿主环境无法嵌套应用 macOS Seatbelt（{}）",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => Some(format!("无法执行 sandbox-exec 探测：{error}")),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let found = std::env::var("TIANGONG_SANDBOX_BWRAP")
            .ok()
            .filter(|path| Path::new(path).exists())
            .is_some()
            || ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
                .iter()
                .any(|path| Path::new(path).exists());
        (!found).then(|| "未找到 bwrap（需要 bubblewrap 且系统允许非特权 user namespace）".into())
    }
    #[cfg(windows)]
    {
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Some("当前平台没有可用的原生沙箱".into())
    }
}

/// 当前测试环境能否终止自己创建的子进程。
///
/// 与 `native_sandbox_skip_reason` 是两个不同能力：外层受限沙箱可能
/// 允许启动子进程但拒绝 process-signal。依赖严格进程回收断言的测试
/// 应根据本结果决定收尾是否执行严格断言——探测失败时打印原因并
/// 返回 false（探测子进程只睡 0.1s，无法终止也不会长期残留）。
#[cfg(unix)]
pub fn can_terminate_child_processes() -> bool {
    let Ok(mut child) = std::process::Command::new("/bin/sleep").arg("0.1").spawn() else {
        return false;
    };

    match child.kill() {
        Ok(()) => {
            let _ = child.wait();
            true
        }
        Err(error) => {
            eprintln!("当前测试环境不能终止子进程，跳过严格清理断言：{error}");
            false
        }
    }
}

/// Windows 无 Seatbelt 式信号过滤，子进程终止能力始终可用；进程组
/// 清理回归由既有 Job Object 测试覆盖。
#[cfg(not(unix))]
pub fn can_terminate_child_processes() -> bool {
    true
}

/// stdio 测试连接的条件收尾：能终止子进程的环境必须严格断言真实清理
/// 成功（失败即测试失败）；受限环境（外层沙箱拒进程信号）允许收尾
/// 失败但打印明确原因——业务断言已全部通过，不因收尾误报。
pub fn finish_stdio_connection(
    connection: &crate::sidecar::StdioSidecarConnection,
    can_terminate: bool,
) {
    match connection.stop() {
        Ok(()) => {}
        Err(error) if !can_terminate => {
            eprintln!("跳过严格进程清理断言：当前环境不允许终止子进程：{error:#}");
        }
        Err(error) => {
            panic!("停止 stdio sidecar 失败：{error:#}");
        }
    }
}

/// 运行时测试专用插件 `runtime-probe` 的制品路径（仓库 `tests/fixtures/plugins/runtime-probe/`）。
///
/// 制品随仓库提交（源码在同目录，见其 README），测试直接加载，不在测试时编译；
/// 缺失或被篡改时直接 panic，不允许静默跳过（否则 CI 会把未执行的测试当成通过）。
pub fn runtime_probe_wasm() -> PathBuf {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures/plugins/runtime-probe");
    let wasm = dir.join("runtime_probe.wasm");
    let bytes = std::fs::read(&wasm)
        .unwrap_or_else(|error| panic!("读取测试插件制品失败 {}: {error}", wasm.display()));
    let expected = std::fs::read_to_string(dir.join("runtime_probe.wasm.sha256"))
        .unwrap_or_else(|error| panic!("读取测试插件制品摘要失败: {error}"));
    let expected = expected.split_whitespace().next().unwrap_or_default();
    let actual = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(&bytes))
    };
    assert_eq!(
        actual,
        expected,
        "测试插件制品摘要不匹配：{}；按同目录 README 重新构建制品并更新摘要",
        wasm.display()
    );
    wasm
}

/// 编译后的 `runtime-probe` 组件：同一测试进程只编译一次，各测试独立实例化。
pub fn runtime_probe_component() -> &'static wasmtime::component::Component {
    static COMPONENT: std::sync::OnceLock<wasmtime::component::Component> =
        std::sync::OnceLock::new();
    COMPONENT.get_or_init(|| {
        let bytes = std::fs::read(runtime_probe_wasm()).expect("读取测试插件制品失败");
        crate::loader::compile_component(&bytes).expect("编译测试插件失败")
    })
}

/// 实例化一个 `runtime-probe` 插件（复用已编译组件）。
pub fn runtime_probe_plugin(
    sidecar: Option<std::sync::Arc<dyn crate::sidecar::SidecarConnection>>,
) -> crate::loader::WasmPlugin {
    crate::loader::instantiate_component(
        runtime_probe_component(),
        &crate::config::PluginRuntimeConfig::default(),
        sidecar,
        "runtime-probe",
        false,
    )
    .expect("实例化测试插件失败")
}
