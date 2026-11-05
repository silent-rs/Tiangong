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

/// 仓库级测试 fixture 目录（`tests/fixtures/`）。runtime 测试只读这里，
/// 不直接引用 `plugins/` 下的插件或 SDK 目录。
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures")
}

/// Node sidecar 协议库测试副本（`tests/fixtures/node-sidecar-sdk/index.mjs`），
/// 供测试拼装 Node 插件；与 `plugins/sdk-sidecar/index.mjs` 的一致性由
/// `tiangong-plugin-sidecar-e2e` 校验。
pub fn node_sidecar_sdk() -> PathBuf {
    let sdk = fixtures_dir().join("node-sidecar-sdk/index.mjs");
    assert!(
        sdk.is_file(),
        "缺少 Node sidecar SDK 测试副本：{}",
        sdk.display()
    );
    sdk
}

/// 写入最小的 plugin-dev 项目元数据（`<存储根>/plugins-dev/<id>/`），
/// 与 devkit init 产出的项目登记形态一致。
pub fn write_dev_project(storage_root: &Path, project_id: &str) -> PathBuf {
    let dir = storage_root
        .join(crate::plugin_dev::PLUGIN_DEV_DIR)
        .join(project_id);
    std::fs::create_dir_all(&dir).expect("创建开发项目目录失败");
    std::fs::write(
        dir.join(crate::plugin_dev::PROJECT_META_FILE),
        serde_json::json!({
            "plugin_id": project_id,
            "name": project_id,
            "template": "ts-npx",
            "created_at": "2026-08-25 00:00:00"
        })
        .to_string(),
    )
    .expect("写入开发项目元数据失败");
    dir
}

/// 安装 plugin-dev 项目的 `release/` 构建产物（跳过受信构建登记核验，
/// 其余签名、完整性与导入链路与正式安装一致）。返回安装的插件 ID。
pub fn install_dev_project(storage_root: &Path, project_id: &str) -> anyhow::Result<String> {
    crate::plugin_dev::install(storage_root, project_id, None).map(|result| result.plugin_id)
}

// ── plugin-dev 测试装置（crate 内测试与 e2e 共用）──

/// 初始化测试配置并准备测试 Launcher 信任链：sidecar 恒走沙箱（策略表
/// 不接受关闭输入），本组装置以测试密钥签名的真实 Launcher 覆盖安装/
/// 签名/调用契约的完整链路。
pub fn init_config_with_launcher(root: &Path) {
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    crate::test_support::ensure_test_launcher_signed(root).unwrap();
}

/// 在 PATH（或 TIANGONG_NODE_PATH）中查找 node 解释器（None 跳过测试）。
pub fn find_node() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TIANGONG_NODE_PATH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "node.exe" } else { "node" }))
        .find(|path| path.is_file())
}

/// 写入最小的 plugin-dev 项目（`<存储根>/plugins-dev/<id>/`：元数据 +
/// 源码清单），与 devkit init 产出的项目登记形态一致。
pub fn make_dev_project(storage_root: &Path, project_id: &str) -> PathBuf {
    let dir = write_dev_project(storage_root, project_id);
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"schema_version":2,"id":"x","version":"0.2.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}}"#
            .replace("\"x\"", &format!("\"{project_id}\"")),
    )
    .unwrap();
    dir
}

/// 构造 devkit 风格的 node sidecar 构建产物（含内容哈希清单）。
pub fn make_node_sidecar_release(storage_root: &Path, project_id: &str) -> PathBuf {
    let release = storage_root
        .join(crate::plugin_dev::PLUGIN_DEV_DIR)
        .join(project_id)
        .join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
        release.join("plugin.json"),
        format!(
            r#"{{"schema_version":2,"id":"{project_id}","version":"0.1.0","permissions":["sidecar.invoke"],"ui":{{"contributions":[{{"slot":"extension.tab","id":"app","entry":"app/index.html"}}]}},"sidecar":{{"runtime":"node","entry":"sidecar/main.mjs"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(release.join("app/index.html"), "<html></html>").unwrap();
    let sdk = node_sidecar_sdk();
    std::fs::copy(
        &sdk,
        release.join("sidecar/vendor/tiangong-sidecar-sdk/index.mjs"),
    )
    .unwrap();
    std::fs::write(
        release.join("sidecar/main.mjs"),
        r#"
import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
await runSidecar({
  pluginId: 'ID_PLACEHOLDER',
  pluginVersion: '0.1.0',
  dispatch(operation, payload) {
    if (operation === 'demo.echo') {
      return { payload: { text: payload?.text ?? '' } };
    }
    if (operation === 'demo.temp') {
      const tempDir = os.tmpdir();
      const target = path.join(tempDir, `tiangong-temp-probe-${process.pid}.txt`);
      fs.writeFileSync(target, 'one');
      const reopened = fs.readFileSync(target, 'utf8') === 'one';
      fs.appendFileSync(target, '-two');
      const modified = fs.readFileSync(target, 'utf8') === 'one-two';
      fs.unlinkSync(target);
      return { payload: { tempDir, reopened, modified, removed: !fs.existsSync(target) } };
    }
    return { payload: {} };
  },
});
"#
        .replace("ID_PLACEHOLDER", project_id),
    )
    .unwrap();
    write_content_manifest(&release);
    release
}

/// devkit build 同款内容清单：全树（排除清单自身）路径 + sha256。
pub fn write_content_manifest(release: &Path) {
    use sha2::Digest;
    let mut files = Vec::new();
    let mut stack = vec![release.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                let path = entry.path();
                let relative = path.strip_prefix(release).unwrap().display().to_string();
                if relative == "content-manifest.json" {
                    continue;
                }
                let raw = std::fs::read(&path).unwrap();
                files.push(serde_json::json!({
                    "path": relative.replace('\\', "/"),
                    "sha256": hex::encode(sha2::Sha256::digest(&raw)),
                }));
            }
        }
    }
    std::fs::write(
        release.join("content-manifest.json"),
        serde_json::json!({"algorithm": "sha256", "files": files}).to_string(),
    )
    .unwrap();
}

/// 递归复制目录树（测试装置用）。
pub fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

/// 恢复受信安装方判定的 fail-closed 初始态（测试隔离）。
pub fn clear_plugin_dev_trusted_installer() {
    crate::plugin_dev::clear_plugin_dev_trusted_installer_for_test();
}

/// 恢复导入确认回调的 fail-closed 初始态（测试隔离）。
pub fn clear_plugin_dev_import_confirm() {
    crate::plugin_dev::clear_plugin_dev_import_confirm_for_test();
}

/// 安装 plugin-dev 项目产物并显式携带受信构建指纹期望（与桥接 call 层
/// 同款核验分支；`expected_fingerprint` 失配即拒绝）。返回安装的插件 ID。
pub fn install_dev_project_checked(
    storage_root: &Path,
    project_id: &str,
    caller: &str,
    expected_fingerprint: &str,
) -> anyhow::Result<String> {
    crate::plugin_dev::install(
        storage_root,
        project_id,
        Some((caller, expected_fingerprint)),
    )
    .map(|result| result.plugin_id)
}
