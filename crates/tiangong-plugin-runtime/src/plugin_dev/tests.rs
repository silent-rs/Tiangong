use super::*;
use serde_json::json;
use std::sync::Arc;

/// 当前环境无法应用原生沙箱时的跳过原因（None 表示可用）。
///
/// 天工终端/受限 CI 等外层 Seatbelt 环境内跑测试时，Launcher 无法
/// 再次嵌套应用沙箱而拒绝启动（退出码 78，安全设计而非缺陷），
/// kill 子进程同样被拒——真实沙箱链路测试在此类环境必然失败或
/// 卡死，统一跳过并打印原因。
fn native_sandbox_skip_reason() -> Option<String> {
    match tiangong_sandbox::sandbox::availability() {
        tiangong_sandbox::sandbox::SandboxAvailability::Available => None,
        tiangong_sandbox::sandbox::SandboxAvailability::EnvironmentRestricted(reason)
        | tiangong_sandbox::sandbox::SandboxAvailability::Unsupported(reason) => Some(reason),
    }
}

/// 真实沙箱链路测试的环境门槛：不可用环境跳过当前测试。
macro_rules! require_native_sandbox {
    () => {
        if let Some(reason) = native_sandbox_skip_reason() {
            eprintln!("跳过真实沙箱测试：当前环境无法应用原生沙箱：{reason}");
            return;
        }
    };
}

/// 初始化测试配置并准备测试 Launcher 信任链：sidecar 恒走沙箱
///（策略表不接受关闭输入），本组测试以测试密钥签名的真实 Launcher
/// 覆盖安装/签名/调用契约的完整链路。
/// 使用此准备步骤的测试以 integration_ 开头，供 nextest 限制重型测试并发。
fn init_config_with_launcher(root: &Path) {
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    crate::test_support::ensure_test_launcher_signed(root).unwrap();
}

fn wait_for_pid_file(path: &Path, timeout: std::time::Duration, context: &str) -> i32 {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(raw) = std::fs::read_to_string(path) {
            return raw
                .trim()
                .parse()
                .unwrap_or_else(|error| panic!("{context} PID 无效: {error}"));
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{context} 未在 {timeout:?} 内生成 PID 文件：{}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn make_project(root: &Path, id: &str) -> PathBuf {
    let dir = root.join(PLUGIN_DEV_DIR).join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join(PROJECT_META_FILE),
            json!({"plugin_id": id, "name": id, "template": "ts-npx", "created_at": "2026-08-25 00:00:00"})
                .to_string(),
        )
        .unwrap();
    std::fs::write(
            dir.join("plugin.json"),
            r#"{"schema_version":2,"id":"x","version":"0.2.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}}"#
                .replace("\"x\"", &format!("\"{id}\"")),
        )
        .unwrap();
    dir
}

#[test]
fn 项目id白名单拒绝路径逃逸() {
    assert!(validate_project_id("my-plugin_1.0").is_ok());
    for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "空 格"] {
        assert!(validate_project_id(bad).is_err(), "应当拒绝 {bad:?}");
    }
}

#[test]
fn list_返回项目与版本状态() {
    let root = tempfile::tempdir().unwrap();
    make_project(root.path(), "demo");
    std::fs::create_dir_all(
        root.path()
            .join(PLUGIN_DEV_DIR)
            .join("demo")
            .join("release"),
    )
    .unwrap();
    std::fs::write(
            root.path().join(PLUGIN_DEV_DIR).join("demo").join("release/plugin.json"),
            r#"{"schema_version":2,"id":"demo","version":"0.1.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}}"#,
        )
        .unwrap();
    let entries = list(root.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "demo");
    assert_eq!(entries[0].source_version.as_deref(), Some("0.2.0"));
    assert_eq!(entries[0].release_version.as_deref(), Some("0.1.0"));
    assert!(entries[0].installed_version.is_none());
}

#[test]
fn status_未初始化返回不存在() {
    let root = tempfile::tempdir().unwrap();
    let result = status(root.path(), "ghost").unwrap();
    assert!(!result.exists);
}

/// 安装完整链（功能验证）：确认桩 → 暂存（不可变副本）→ 用户密钥签名
/// → 导入 → 安装目录与注册表就位；确认信息（版本）来自暂存副本而非
/// 可变的 release/。无 sidecar 的纯 UI/工具模板同样获得 local 签名
/// （动态调用通道的分流判据，P0 回归）。
#[test]
#[serial_test::serial]
fn install_完整链_暂存确认导入与注册表() {
    let root = tempfile::tempdir().unwrap();
    tiangong_config::registry::init_from_dir(root.path());
    let project = root.path().join(PLUGIN_DEV_DIR).join("inst-demo");
    std::fs::create_dir_all(project.join("release/dist")).unwrap();
    std::fs::write(
        project.join(PROJECT_META_FILE),
        r#"{"plugin_id":"inst-demo","name":"安装链验证","template":"ts-npx","created_at":"t"}"#,
    )
    .unwrap();
    std::fs::write(
            project.join("release/plugin.json"),
            r#"{"schema_version":2,"id":"inst-demo","version":"9.9.9","permissions":[],"entrypoints":["desktop"],"capabilities":{"prompt":true},"prompt":["能力说明"],"mention":{"hint":"安装链验证能力"},"ui":{"contributions":[{"slot":"extension.tab","id":"app","title":"安装链验证","entry":"dist/index.html"}]}}"#,
        )
        .unwrap();
    std::fs::write(project.join("release/dist/index.html"), "<html></html>").unwrap();
    write_content_manifest(&project.join("release"));

    let result = install(root.path(), "inst-demo", None).expect("安装完整链");
    assert_eq!(result.plugin_id, "inst-demo");
    assert_eq!(result.version, "9.9.9");
    assert!(result.enabled, "安装后应启用");
    // 安装目录与注册表
    assert!(root.path().join("plugins/inst-demo/plugin.json").is_file());
    assert!(
        root.path()
            .join("plugins/inst-demo/dist/index.html")
            .is_file()
    );
    // 无 sidecar 模板同样拿到 local 签名（动态调用通道判据）。
    assert!(
        root.path().join("plugins/inst-demo/release.json").is_file(),
        "纯 UI/工具模板安装后应有签名清单"
    );
    assert!(
        crate::registry::is_local_plugin("inst-demo"),
        "无 sidecar 自制插件应命中 local 判据"
    );
    assert!(
        crate::registry::plugin_manifest("inst-demo").is_some(),
        "注册表应可见已装插件"
    );
    // @提及候选实时聚合：安装后立即可见（不依赖会话 Core 快照）。
    let mentions = crate::registry::mention_sources()
        .into_iter()
        .flat_map(|source| {
            source
                .query(&tiangong_types::MentionQuery::default())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        mentions
            .iter()
            .any(|candidate| candidate.value == "@plugin:inst-demo"),
        "安装后 mention 候选应立即可见：{mentions:?}"
    );
}

#[test]
fn install_缺构建产物时给出devkit指引() {
    let root = tempfile::tempdir().unwrap();
    make_project(root.path(), "demo");
    let error = install(root.path(), "demo", None).unwrap_err();
    assert!(
        error.to_string().contains("@silent-ai/plugin-creator"),
        "应指引 devkit 命令：{error}"
    );
}

fn find_node_for_test() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TIANGONG_NODE_PATH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "node.exe" } else { "node" }))
        .find(|path| path.is_file())
}

/// 构造 devkit 风格的 node sidecar 构建产物（含内容哈希清单）。
fn make_node_sidecar_release(root: &Path, id: &str) -> PathBuf {
    let release = root.join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            format!(
                r#"{{"schema_version":2,"id":"{id}","version":"0.1.0","permissions":["sidecar.invoke"],"ui":{{"contributions":[{{"slot":"extension.tab","id":"app","entry":"app/index.html"}}]}},"sidecar":{{"runtime":"node","entry":"sidecar/main.mjs"}}}}"#
            ),
        )
        .unwrap();
    std::fs::write(release.join("app/index.html"), "<html></html>").unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs");
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
        .replace("ID_PLACEHOLDER", id),
    )
    .unwrap();
    write_content_manifest(&release);
    release
}

/// devkit build 同款内容清单：release 全树（排除清单自身）路径 + sha256。
fn write_content_manifest(release: &Path) {
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
                files.push(json!({
                    "path": relative.replace('\\', "/"),
                    "sha256": hex::encode(sha2::Sha256::digest(&raw)),
                }));
            }
        }
    }
    std::fs::write(
        release.join("content-manifest.json"),
        json!({"algorithm": "sha256", "files": files}).to_string(),
    )
    .unwrap();
}

fn copy_tree_for_test(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree_for_test(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

/// 按需 sidecar 的沙箱写域随新版统一调用上下文的工作区构造：真实
/// 插件写会话工作区成功，写存储根之外的路径被沙箱拒绝。
#[test]
#[serial_test::serial]
fn integration_按需sidecar_新版上下文工作区成为沙箱写域() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "workspace-probe";
    make_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            r#"{"schema_version":2,"id":"workspace-probe","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide","sidecar.invoke"],"capabilities":{"tools":true},"tools":[{"name":"write_probe","description":"写域探测","input_schema":{"type":"object"},"timeout_ms":30000}],"sidecar":{"runtime":"node","entry":"sidecar/main.mjs"}}"#,
        )
        .unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs");
    std::fs::copy(
        &sdk,
        release.join("sidecar/vendor/tiangong-sidecar-sdk/index.mjs"),
    )
    .unwrap();
    std::fs::write(
        release.join("sidecar/main.mjs"),
        r#"
import { writeFileSync } from 'node:fs';
import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
await runSidecar({
  pluginId: 'workspace-probe',
  dispatch(operation, payload) {
    if (operation === 'write_probe') {
      try {
        writeFileSync(payload.target, 'probe');
        return { payload: { ok: true } };
      } catch (error) {
        return { payload: { ok: false, error: String(error) } };
      }
    }
    return { payload: {} };
  },
});
"#,
    )
    .unwrap();
    write_content_manifest(&release);
    install(root.path(), id, None).expect("安装写域探测插件");

    // 会话工作区与外部目录（存储根之外）。
    let workspace = root.path().join("session-workspace");
    std::fs::create_dir(&workspace).unwrap();
    let probe_call = |target: &std::path::Path| tiangong_llm::tool::ToolCall {
        id: "call-probe".to_string(),
        name: "write_probe".to_string(),
        arguments: json!({"target": target.display().to_string()}),
    };
    let invoke_probe = |call: tiangong_llm::tool::ToolCall| {
        let invocation = crate::invocation::RuntimeInvocation::new(
            id,
            call,
            "session-e2e",
            workspace.display().to_string(),
            "agent",
            None,
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                crate::ts_plugin::invoke_sidecar_tool_with_invocation_for_test(
                    id,
                    invocation.call().clone(),
                    invocation,
                ),
            )
    };

    let result = invoke_probe(probe_call(&workspace.join("in-session.txt")));
    assert!(result.ok, "写会话工作区应成功: {}", result.summary);
    assert!(
        workspace.join("in-session.txt").is_file(),
        "真实插件产物应落在当前会话工作区"
    );

    // 写系统临时目录之外、工作区与存储根之外的路径（卷根目录）：
    // 沙箱拒绝。系统 temp 目录是既有全局开放域（lance spill 等），
    // 不作为越界目标。
    let outside_target = if cfg!(unix) {
        std::path::PathBuf::from("/tiangong-write-escape-probe.txt")
    } else {
        std::path::PathBuf::from("C:\\tiangong-write-escape-probe.txt")
    };
    let result = invoke_probe(probe_call(&outside_target));
    assert!(
        !result.ok,
        "写工作区之外的路径应被沙箱拒绝: {}",
        result.summary
    );
    assert!(!outside_target.exists());
}

/// sidecar 验证记录的安装链闭环：安装即生成记录（真实握手采集）、
/// 记录缺失时重新验证入口恢复、卸载清理记录。
#[test]
#[serial_test::serial]
fn integration_sidecar验证记录_安装生成_重验恢复_卸载清理() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "verify-record-demo";
    make_project(root.path(), id);
    make_node_sidecar_release(root.path(), id);
    install(root.path(), id, None).expect("安装应完成 sidecar 完整验证并生成记录");

    let plugin_directory = root.path().join("plugins").join(id);
    let manifest = installed_plugin_manifest(root.path(), id).expect("已安装清单");
    // 安装阶段的真实握手未声明工具能力：记录有效且能力为空（有 UI
    // 插件据此保持 UI Handler 路由）。
    assert_eq!(
        crate::verification::load_valid_capabilities(&plugin_directory, &manifest),
        Some(Vec::new()),
        "安装应生成有效验证记录"
    );
    // 严格安装语义：安装事务提交后当前进程的内存能力快照立即可见，
    // 不等待应用重启或后台补验证。
    assert_eq!(
        crate::registry::loaded_verified_sidecar(id),
        Some(Vec::new()),
        "安装成功必须立即刷新当前进程的验证能力"
    );

    // 记录缺失（旧插件/被删除）：加载视为无效，重新验证入口恢复。
    std::fs::remove_file(
        plugin_directory
            .parent()
            .unwrap()
            .join(".verifications")
            .join(format!("{id}.json")),
    )
    .unwrap();
    assert!(crate::verification::load_valid_capabilities(&plugin_directory, &manifest).is_none());
    let verification_path = crate::verification::verification_path(&plugin_directory).unwrap();
    // 握手成功但验证记录无法保存时，启动结果必须带出该失败；恢复写入后同入口可重试。
    std::fs::create_dir(&verification_path).unwrap();
    let readiness = crate::registry::prepare_desktop_startup_plugins(root.path())
        .expect("插件级失败不再阻断启动");
    assert!(
        readiness
            .failures
            .iter()
            .any(|failure| failure.contains("保存 sidecar 验证记录失败")),
        "{readiness:?}"
    );
    std::fs::remove_dir(&verification_path).unwrap();
    let readiness =
        crate::registry::prepare_desktop_startup_plugins(root.path()).expect("启动重试应恢复记录");
    assert!(
        readiness.failures.is_empty(),
        "重试成功后不应残留插件失败：{readiness:?}"
    );
    assert!(
        crate::verification::load_valid_capabilities(&plugin_directory, &manifest).is_some(),
        "重新验证后记录应恢复"
    );

    // 卸载：记录随插件清理。
    crate::registry::uninstall_plugin(root.path(), id, false).expect("卸载插件");
    assert!(
        !plugin_directory
            .parent()
            .unwrap()
            .join(".verifications")
            .join(format!("{id}.json"))
            .exists(),
        "卸载应删除验证记录"
    );
}

#[test]
#[serial_test::serial]
fn integration_解释器sidecar_创作链自动签名安装_真实调用与篡改拒绝() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "node-sc-demo";
    make_project(root.path(), id);
    make_node_sidecar_release(root.path(), id);

    let result = install(root.path(), id, None).expect("解释器 sidecar 安装");
    assert_eq!(result.plugin_id, id);

    // 真实调用：宿主 stdio 连接 ↔ node 解释器 sidecar。
    let response =
        crate::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({"text": "本地信任"}))
            .expect("sidecar 调用");
    assert_eq!(response["text"], "本地信任");

    // 篡改安装目录内的入口脚本：内容清单复核必须拒绝。
    let installed_entry = root
        .path()
        .join("plugins")
        .join(id)
        .join("sidecar/main.mjs");
    std::fs::write(&installed_entry, "// tampered\n").unwrap();
    let error = crate::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({}))
        .expect_err("篡改后应拒绝启动");
    assert!(
        format!("{error:#}").contains("篡改"),
        "应报篡改错误：{error:#}"
    );
}

#[test]
fn 解释器sidecar_无有效签名时拒绝启动() {
    let root = tempfile::tempdir().unwrap();
    let id = "node-sc-notrusted";
    make_project(root.path(), id);
    let release = make_node_sidecar_release(root.path(), id);
    // 手动把产物放进安装目录，不经 plugin-dev 确认链（无 local-trust.json）。
    let installed = root.path().join("plugins").join(id);
    copy_tree_for_test(&release, &installed);
    let error = crate::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({}))
        .expect_err("无有效签名应拒绝");
    assert!(
        format!("{error:#}").contains("需签名安装"),
        "应提示签名安装路径：{error:#}"
    );
}

/// 安装授权五场景：fail-closed、非受信插件、非官方签名的固定 ID、
/// 缺构建登记、受信齐全。
/// 调用方插件为已安装的 node demo 插件（install 本体直调装好）。
#[test]
#[serial_test::serial]
fn integration_安装授权_五场景() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let caller = "node-sc-auth-caller";
    make_project(root.path(), caller);
    make_node_sidecar_release(root.path(), caller);
    // 待安装的项目产物。
    let target = "node-sc-auth-target";
    make_project(root.path(), target);
    make_node_sidecar_release(root.path(), target);
    // 调用方插件本体先经 install 直调装好（其自身安装不经桥接授权）。
    install(root.path(), caller, None).expect("调用方插件安装");

    let payload = |id: &str| serde_json::json!({ "id": id }).to_string();

    // 1) 未注入受信判定：fail-closed。
    clear_plugin_dev_trusted_installer_for_test();
    let error = call(caller, "plugin-dev.install", &payload(target))
        .expect_err("未注入受信判定应 fail-closed");
    assert!(format!("{error:#}").contains("fail-closed"), "{error:#}");

    // 2) 非受信插件（判定只认别的插件）。
    set_plugin_dev_trusted_installer(Arc::new(|identity: &InstallerIdentity| {
        identity.plugin_id == "someone-else"
    }));
    let error = call(caller, "plugin-dev.install", &payload(target)).expect_err("非受信插件应拒绝");
    assert!(
        format!("{error:#}").contains("无自动签名安装资格"),
        "{error:#}"
    );

    // 3) 受信但缺构建登记。
    let expected_caller = caller.to_string();
    set_plugin_dev_trusted_installer(Arc::new(move |identity: &InstallerIdentity| {
        identity.plugin_id == expected_caller
    }));
    let error = call(caller, "plugin-dev.install", &payload(target)).expect_err("缺构建登记应拒绝");
    assert!(
        format!("{error:#}").contains("缺少受信构建登记"),
        "{error:#}"
    );

    // 5) 受信 + 已登记（指纹锚定构建产物）：完整签名安装链成功。
    let target_fingerprint = {
        use sha2::Digest;
        let manifest_raw = std::fs::read(
            root.path()
                .join(PLUGIN_DEV_DIR)
                .join(target)
                .join("release/content-manifest.json"),
        )
        .unwrap();
        hex::encode(sha2::Sha256::digest(&manifest_raw))
    };
    note_trusted_build(caller, target, Some(target_fingerprint.clone()));
    let result =
        call(caller, "plugin-dev.install", &payload(target)).expect("受信且已登记应安装成功");
    assert!(result.contains(target), "{result}");
    // 清理全局态，避免污染其他测试。
    clear_plugin_dev_trusted_installer_for_test();
}

/// 指纹核验：构建登记后篡改产物（重算内容清单）再经桥接安装必须拒绝；
/// 成功安装消费登记（再次安装需重新构建）。
#[test]
#[serial_test::serial]
fn integration_指纹核验_篡改产物拒绝与安装消费() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let caller = "node-sc-fp-caller";
    let target = "node-sc-fp-target";
    make_project(root.path(), caller);
    make_node_sidecar_release(root.path(), caller);
    make_project(root.path(), target);
    make_node_sidecar_release(root.path(), target);
    install(root.path(), caller, None).expect("调用方插件安装");

    let expected_caller = caller.to_string();
    set_plugin_dev_trusted_installer(Arc::new(move |identity: &InstallerIdentity| {
        identity.plugin_id == expected_caller
    }));
    let release_dir = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join(target)
        .join("release");
    let fingerprint = content_manifest_fingerprint(&release_dir).unwrap();
    note_trusted_build(caller, target, Some(fingerprint));

    // 构建后篡改产物（改文件并重算内容清单）：指纹失配拒绝。
    std::fs::write(
        release_dir.join("sidecar/main.mjs"),
        "// tampered after build",
    )
    .unwrap();
    write_content_manifest(&release_dir);
    let error = call(
        caller,
        "plugin-dev.install",
        &serde_json::json!({ "id": target }).to_string(),
    )
    .expect_err("篡改产物应拒绝安装");
    assert!(format!("{error:#}").contains("不一致"), "{error:#}");

    // 恢复产物并重新登记（等价重新构建）：安装成功且登记被消费。
    make_node_sidecar_release(root.path(), target);
    let fingerprint = content_manifest_fingerprint(&release_dir).unwrap();
    note_trusted_build(caller, target, Some(fingerprint));
    call(
        caller,
        "plugin-dev.install",
        &serde_json::json!({ "id": target }).to_string(),
    )
    .expect("登记匹配应安装成功");
    assert!(
        trusted_build_fingerprint(caller, target).is_none(),
        "安装成功后登记应被消费"
    );
    // 消费后再次安装：要求重新构建。
    let error = call(
        caller,
        "plugin-dev.install",
        &serde_json::json!({ "id": target }).to_string(),
    )
    .expect_err("消费后应要求重新构建");
    assert!(
        format!("{error:#}").contains("缺少受信构建登记"),
        "{error:#}"
    );
    clear_plugin_dev_trusted_installer_for_test();
}

/// 失败构建撤销：note(None) 语义清除旧登记后不可安装。
#[test]
#[serial_test::serial]
fn integration_失败构建_撤销登记后不可安装() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let caller = "node-sc-revoke-caller";
    let target = "node-sc-revoke-target";
    make_project(root.path(), caller);
    make_node_sidecar_release(root.path(), caller);
    make_project(root.path(), target);
    make_node_sidecar_release(root.path(), target);
    install(root.path(), caller, None).expect("调用方插件安装");

    let expected_caller = caller.to_string();
    set_plugin_dev_trusted_installer(Arc::new(move |identity: &InstallerIdentity| {
        identity.plugin_id == expected_caller
    }));
    let release_dir = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join(target)
        .join("release");
    let fingerprint = content_manifest_fingerprint(&release_dir).unwrap();
    note_trusted_build(caller, target, Some(fingerprint));
    // 构建失败：观察者撤销登记。
    note_trusted_build(caller, target, None);
    let error = call(
        caller,
        "plugin-dev.install",
        &serde_json::json!({ "id": target }).to_string(),
    )
    .expect_err("撤销后应拒绝安装");
    assert!(
        format!("{error:#}").contains("缺少受信构建登记"),
        "{error:#}"
    );
    clear_plugin_dev_trusted_installer_for_test();
}

/// sidecar 结果观察者：bridge 的 sidecar.* 成功调用触发（宿主溯源登记
/// 的机制基础）。
#[test]
#[serial_test::serial]
fn integration_sidecar观察者_成功调用触发() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "node-sc-observer";
    make_project(root.path(), id);
    make_node_sidecar_release(root.path(), id);
    install(root.path(), id, None).expect("安装");

    let observed = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let sink = Arc::clone(&observed);
    crate::bridge::set_sidecar_result_observer(Arc::new(
        move |plugin_id: &str, operation: &str, payload: &str, result: &str| {
            let mut records = sink.lock().unwrap();
            records.push((
                format!("{plugin_id}|{operation}|{payload}|{result}"),
                String::new(),
            ));
        },
    ));
    let response = crate::bridge_call(
        id,
        "sidecar.demo.echo",
        &serde_json::json!({"text": "obs"}).to_string(),
    )
    .expect("sidecar 桥接调用");
    assert!(response.contains("obs"), "{response}");
    #[cfg(windows)]
    {
        let invoke_temp = || {
            let response = crate::bridge_call(id, "sidecar.demo.temp", "{}")
                .expect("Windows AppContainer 私有 Temp 文件操作");
            serde_json::from_str::<serde_json::Value>(&response).unwrap()
        };
        let payload = invoke_temp();
        let temp_dir = PathBuf::from(payload["tempDir"].as_str().unwrap());
        let normalized = temp_dir.to_string_lossy().replace('\\', "/").to_lowercase();
        assert!(
            normalized.contains("/packages/tiangongsandbox.") && normalized.ends_with("/ac/temp"),
            "Windows 沙箱未使用 AppContainer 私有 Temp: {temp_dir:?}"
        );
        assert_eq!(payload["reopened"], true);
        assert_eq!(payload["modified"], true);
        assert_eq!(payload["removed"], true);
        let next = invoke_temp();
        assert_ne!(
            payload["tempDir"], next["tempDir"],
            "不同临时沙箱身份不得复用同一私有 Temp"
        );
    }
    let records = observed.lock().unwrap();
    assert!(
        records
            .iter()
            .any(|(record, _)| record.contains(&format!("{id}|demo.echo|"))),
        "观察者应收到成功调用记录：{records:?}"
    );
}

/// 三方导入链：签名归档 → 导入发布者公钥 → 暂存解包 → 签名验证安装
/// → sidecar 真实调用 → 移除公钥后失效。
#[test]
#[serial_test::serial]
fn integration_三方导入_签名归档安装与移除公钥后失效() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "node-sc-third-party";
    make_project(root.path(), id);
    let release = make_node_sidecar_release(root.path(), id);

    // 三方开发者视角：以自己的密钥为插件签发签名清单（发布者 acme-dev）。
    let sha256_of = |path: std::path::PathBuf| {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(std::fs::read(&path).unwrap()))
    };
    let release_json = json!({
        "schema_version": 1,
        "id": id,
        "version": "0.1.0",
        "publisher": "acme-dev",
        "permissions": ["sidecar.invoke"],
        "manifest": {
            "path": "plugin.json",
            "sha256": sha256_of(release.join("plugin.json")),
        },
        "ui": [{
            "path": "app/index.html",
            "sha256": sha256_of(release.join("app/index.html")),
        }],
        "content_manifest": {
            "path": "content-manifest.json",
            "sha256": sha256_of(release.join("content-manifest.json")),
        },
    });
    std::fs::write(
        release.join("release.json"),
        serde_json::to_vec_pretty(&release_json).unwrap(),
    )
    .unwrap();
    let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(
        Some(&keypair.pk),
        &keypair.sk,
        serde_json::to_vec_pretty(&release_json).unwrap().as_slice(),
        None,
        None,
    )
    .unwrap();
    use base64::Engine;
    std::fs::write(
        release.join("release.json.sig"),
        base64::engine::general_purpose::STANDARD.encode(signature.into_string()),
    )
    .unwrap();

    // 打三方分发归档（tar.zst，含签名清单与内容清单，排除 local-trust）。
    let archive = root.path().join(format!("{id}-0.1.0.tar.zst"));
    {
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = zstd::Encoder::new(file, 3).unwrap();
        let mut builder = tar::Builder::new(encoder);
        let mut stack = vec![release.clone()];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let name = entry.file_name();
                if directory == release
                    && matches!(name.to_string_lossy().as_ref(), "local-trust.json")
                {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    stack.push(path);
                    continue;
                }
                let relative = path.strip_prefix(&release).unwrap();
                builder.append_path_with_name(&path, relative).unwrap();
            }
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    // 未导入公钥：归档可解包暂存，但安装（签名验证）拒绝。
    let staged = crate::artifacts::stage_plugin_archive(root.path(), &archive).expect("归档暂存");
    let error = crate::registry::import_staged_plugin(root.path(), staged.path())
        .expect_err("未导入三方公钥应拒绝安装");
    assert!(
        format!("{error:#}").contains("未导入"),
        "应给出公钥导入指引：{error:#}"
    );
    // 显式释放失败暂存（shadowing 的旧绑定活到作用域结束才 Drop，
    // 会干扰下方事务目录断言；生产路径失败返回即释放）。
    drop(staged);

    // 导入公钥后：安装 → 真实调用成功。
    let public_b64 = base64::engine::general_purpose::STANDARD
        .encode(keypair.pk.to_box().unwrap().into_string());
    crate::trust::import_trusted_publisher(root.path(), "acme-dev", &public_b64).unwrap();
    let staged = crate::artifacts::stage_plugin_archive(root.path(), &archive).expect("归档暂存");
    crate::registry::import_staged_plugin(root.path(), staged.path()).expect("三方签名插件安装");
    let response =
        crate::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({"text": "三方"}))
            .expect("三方插件 sidecar 调用");
    assert_eq!(response["text"], "三方");
    // 安装成功后事务目录不应累积残留（数据保留壳/坏残留即清）。
    let transactions = root.path().join("plugins").join(".transactions");
    let leftovers: Vec<_> = std::fs::read_dir(&transactions)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect();
    assert!(
        leftovers.is_empty(),
        "事务目录残留：{:?}",
        leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
    );

    // 移除公钥：下次启动即失效（重新发现验证时公钥缺失）。
    crate::trust::remove_trusted_publisher(root.path(), "acme-dev").unwrap();
    let error = crate::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({}))
        .expect_err("移除公钥后应拒绝启动");
    assert!(
        format!("{error:#}").contains("未导入"),
        "应报公钥缺失：{error:#}"
    );
}

/// 官方签名与本地信任混用拒绝：本地信任安装后再落入官方签名文件，
/// 两种信任来源同时存在时启动门槛必须拒绝（来源不明确）。
#[test]
#[serial_test::serial]
fn integration_解释器sidecar_官方签名与本地信任混用拒绝() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "node-sc-mixed-trust";
    make_project(root.path(), id);
    make_node_sidecar_release(root.path(), id);
    install(root.path(), id, None).expect("签名链安装");

    // 遗留本地信任插件（升级前存量形态）+ 官方签名同存：制造双信任
    // 来源。安装链已不落锚，这里手工补遗留标记。
    let installed = root.path().join("plugins").join(id);
    {
        use sha2::Digest;
        let manifest_raw = std::fs::read(installed.join("content-manifest.json")).unwrap();
        let anchor = hex::encode(sha2::Sha256::digest(&manifest_raw));
        std::fs::write(
            installed.join("local-trust.json"),
            format!(r#"{{"kind":"local-confirm","content_sha256":"{anchor}"}}"#),
        )
        .unwrap();
    }

    // 在已带遗留本地信任标记的安装目录上追加真实有效的签名
    // （解释器形态，内容清单哈希与目录一致）。

    let sha256_of = |path: std::path::PathBuf| {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(std::fs::read(&path).unwrap()))
    };
    let release = json!({
        "schema_version": 1,
        "id": id,
        "version": "0.1.0",
        "publisher": "acme-dev",
        "permissions": ["sidecar.invoke"],
        "manifest": {
            "path": "plugin.json",
            "sha256": sha256_of(installed.join("plugin.json")),
        },
        "ui": [{
            "path": "app/index.html",
            "sha256": sha256_of(installed.join("app/index.html")),
        }],
        "content_manifest": {
            "path": "content-manifest.json",
            "sha256": sha256_of(installed.join("content-manifest.json")),
        },
    });
    let release_raw = serde_json::to_vec_pretty(&release).unwrap();
    std::fs::write(installed.join("release.json"), &release_raw).unwrap();
    let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(
        Some(&keypair.pk),
        &keypair.sk,
        release_raw.as_slice(),
        None,
        None,
    )
    .unwrap();
    // 与 tauri signer 输出一致：两行签名文本整体 base64（verify_minisign 格式）。
    use base64::Engine;
    std::fs::write(
        installed.join("release.json.sig"),
        base64::engine::general_purpose::STANDARD.encode(signature.into_string()),
    )
    .unwrap();
    // 三方发布者公钥经登记表导入（官方信任根不可配置，不适用于本测试）。
    let pubkey_b64 = base64::engine::general_purpose::STANDARD
        .encode(keypair.pk.to_box().unwrap().into_string());
    crate::trust::import_trusted_publisher(root.path(), "acme-dev", &pubkey_b64).unwrap();
    let error = crate::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({}))
        .expect_err("混用信任来源应拒绝启动");
    assert!(
        format!("{error:#}").contains("同时携带官方签名与本地信任标记"),
        "应报混用拒绝：{error:#}"
    );
}

/// 真实 creator 产物全链路：package → plugin-dev 安装（原生确认 + 暂存 +
/// 双向完整性 + 本地信任落锚）→ 按需 sidecar 真实执行 devkit.init（验证
/// templates 随行资源经 resources 声明进入安装目录并被 devkit 使用）。
/// 产物（release/）由 `yarn package` 生成、不入库：CI 无产物时跳过。
#[test]
#[serial_test::serial]
fn integration_creator真实产物_安装与devkit_init全链路() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let release_source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/tiangong-plugin-creator/release");
    if !release_source.join("plugin.json").is_file() {
        eprintln!(
            "跳过：缺少真实构建产物 {}（先 yarn package）",
            release_source.display()
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    // 安装链的可用性探测读取全局模型配置；测试进程用隔离目录初始化。
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "plugin-creator";
    let dev_root = root.path().join(PLUGIN_DEV_DIR).join(id);
    copy_tree_for_test(&release_source, &dev_root.join("release"));
    make_project(root.path(), id);

    let result = install(root.path(), id, None).expect("creator 真实产物安装");
    assert_eq!(result.plugin_id, id);

    // 按需 sidecar 真实执行 devkit.init：安装目录内的 templates 必须可用。
    let init_root = root.path().join("init-output");
    let response = crate::registry::invoke_sidecar(
        root.path(),
        id,
        "devkit.init",
        json!({"args": ["ui-app", "chain-probe", "--name", "链路探针"], "root": init_root}),
    )
    .expect("devkit.init 经按需 sidecar 执行");
    assert_eq!(
        response["ok"],
        json!(true),
        "devkit.init 应成功: {response}"
    );
    assert!(
        init_root.join("chain-probe/plugin.json").is_file(),
        "模板项目应真实创建（templates 随行资源可用）"
    );
}
/// 完整用户旅程（显式运行：`cargo test -p tiangong-plugin-runtime --lib
/// -- --ignored`）：经 creator 的按需 sidecar 从零创建 node-sidecar 插件
/// → 注入自定义操作 → devkit 真实构建（yarn 工程链）→ 自动签名安装 →
/// 宿主连接新插件的 sidecar 真实调用。全程不经 GUI，等价于 Agent 操作序列。
#[test]
#[ignore = "真实 yarn 构建需数分钟与网络，按需显式运行"]
#[serial_test::serial]
fn integration_从零创建node_sidecar插件_完整旅程() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let release_source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/tiangong-plugin-creator/release");
    if !release_source.join("plugin.json").is_file() {
        eprintln!("跳过：缺少真实构建产物（先在 creator 目录 yarn package）");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let creator = "plugin-creator";
    let dev_root = root.path().join(PLUGIN_DEV_DIR).join(creator);
    copy_tree_for_test(&release_source, &dev_root.join("release"));
    make_project(root.path(), creator);
    install(root.path(), creator, None).expect("creator 安装");

    let devkit = |command: &str, args: serde_json::Value| {
        crate::registry::invoke_sidecar(root.path(), creator, &format!("devkit.{command}"), args)
    };

    // 1) 初始化 node-sidecar 模板项目（真实模板，写入受控开发根）。
    let project_id = "journey-demo";
    let response = devkit(
            "init",
            json!({"args": ["node-sidecar", project_id, "--name", "旅程验证"], "root": root.path().join(PLUGIN_DEV_DIR)}),
        )
        .expect("devkit.init");
    assert_eq!(response["ok"], json!(true), "init 失败: {response}");

    // 2) 修改 sidecar 源码：注入自定义操作（模拟用户/Agent 编码）。
    let entry = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join(project_id)
        .join("sidecar/main.mjs");
    let source = std::fs::read_to_string(&entry).unwrap();
    let patched = source.replace(
            "    if (operation === 'demo.echo') {",
            "    if (operation === 'journey.greet') {\n      return { payload: { message: `你好，${payload?.who ?? '天工'}！` } };\n    }\n    if (operation === 'demo.echo') {",
        );
    assert!(patched != source, "应成功注入自定义操作");
    std::fs::write(&entry, patched).unwrap();

    // 3) 真实构建（yarn install + 类型检查 + vite/esbuild 双端打包）。
    let response = devkit(
        "build",
        json!({"args": [project_id], "root": root.path().join(PLUGIN_DEV_DIR)}),
    )
    .expect("devkit.build");
    assert_eq!(response["ok"], json!(true), "build 失败: {response}");

    // 4) 安装新插件（原生确认通道 + 完整性 + 本地信任）。
    let result = install(root.path(), project_id, None).expect("新插件安装");
    assert_eq!(result.plugin_id, project_id);

    // 5) 宿主连接新插件 sidecar，真实调用自定义操作。
    let response = crate::registry::invoke_sidecar(
        root.path(),
        project_id,
        "journey.greet",
        json!({"who": "完整旅程"}),
    )
    .expect("自定义操作调用");
    assert_eq!(
        response["message"],
        json!("你好，完整旅程！"),
        "响应: {response}"
    );
}
/// 与任何真实指纹都不可能相等的占位（拒绝路径用）。
fn unreachable_fingerprint() -> String {
    "0".repeat(64)
}

/// 纯 UI 插件的构建指纹核验（无 sidecar 形态同样受登记约束）：构建后
/// 修改产物被拒，重新登记（等价重新构建）后放行。
#[test]
#[serial_test::serial]
fn integration_纯ui插件_构建指纹核验() {
    let root = tempfile::tempdir().unwrap();
    // 纯 UI 产物无 sidecar：安装不触发沙箱内验证，无需原生沙箱环境。
    init_config_with_launcher(root.path());
    let id = "ui-fp-demo";
    make_project(root.path(), id);
    // 纯 UI 产物：ui 贡献 + 内容清单，无 sidecar（devkit 全模板统一生成清单）。
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            format!(
                r#"{{"schema_version":2,"id":"{id}","version":"0.1.0","permissions":[],"ui":{{"contributions":[{{"slot":"extension.tab","id":"app","entry":"app/index.html"}}]}}}}"#
            ),
        )
        .unwrap();
    std::fs::write(release.join("app/index.html"), "<html></html>").unwrap();
    write_content_manifest(&release);

    // 受信调用方（模拟宿主判定）+ 指纹登记后经桥接安装成功。
    let fingerprint = content_manifest_fingerprint(&release).unwrap();
    note_trusted_build("plugin-creator", id, Some(fingerprint));
    // 桥接调用要求调用方插件已加载——本测试聚焦指纹路径，直调 install
    // 并显式传入登记期望（与 call 层同款核验分支）。
    let install_result = install(
        root.path(),
        id,
        Some(("plugin-creator", &unreachable_fingerprint())),
    );
    // 上面故意用失配指纹断言拒绝路径：
    assert!(
        install_result.is_err_and(|error| format!("{error:#}").contains("不一致")),
        "纯 UI 产物同样受指纹核验约束"
    );
    // 正确指纹放行。
    let release_dir = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    let fingerprint = content_manifest_fingerprint(&release_dir).unwrap();
    let result = install(root.path(), id, Some(("plugin-creator", &fingerprint)))
        .expect("纯 UI 指纹匹配应安装成功");
    assert_eq!(result.plugin_id, id);
}

/// 无界面纯工具插件（node-tool 形态）：无 ui 贡献即可安装（校验解耦），
/// 工具契约（操作名=工具名、ToolOutcome 形状）经 sidecar 真实往返。
#[test]
#[serial_test::serial]
fn integration_无界面纯工具插件_安装与工具契约() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "pure-tool-demo";
    make_project(root.path(), id);
    // 构造 node-tool 形态产物：tools + sidecar，无 ui/wasm。
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            r#"{"schema_version":2,"id":"pure-tool-demo","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide","sidecar.invoke"],"capabilities":{"tools":true},"tools":[{"name":"text_analyze","description":"文本分析","input_schema":{"type":"object"},"timeout_ms":20000}],"sidecar":{"runtime":"node","entry":"sidecar/main.mjs"},"mention":{"hint":"纯工具"}}"#,
        )
        .unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs");
    std::fs::copy(
        &sdk,
        release.join("sidecar/vendor/tiangong-sidecar-sdk/index.mjs"),
    )
    .unwrap();
    std::fs::write(
            release.join("sidecar/main.mjs"),
            r#"
import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
await runSidecar({
  pluginId: 'pure-tool-demo',
  dispatch(operation, payload) {
    if (operation === 'text_analyze') {
      const text = typeof payload?.text === 'string' ? payload.text : '';
      return { payload: { ok: true, summary: `文本 ${[...text].length} 字`, stdout: [...text].reverse().join(''), stderr: '', exit_code: 0 } };
    }
    return { payload: {} };
  },
});
"#,
        )
        .unwrap();
    write_content_manifest(&release);

    let result = install(root.path(), id, None).expect("无界面纯工具插件应可安装");
    assert_eq!(result.plugin_id, id);

    // 工具契约：操作名 = 工具名，参数 = 工具参数对象，返回 ToolOutcome。
    let response = crate::registry::invoke_sidecar(
        root.path(),
        id,
        "text_analyze",
        json!({"text": "天工abc"}),
    )
    .expect("工具直连调用");
    assert_eq!(response["ok"], json!(true), "响应: {response}");
    assert_eq!(response["summary"], json!("文本 5 字"));
    assert_eq!(response["stdout"], json!("cba工天"));
}
/// 定位示例 memory wasm 组件（与 tests/load_and_call.rs 同一产物）。
fn memory_wasm_or_skip() -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target/wasm32-wasip2/debug/tiangong_plugin_memory_wasm.wasm");
    if !path.exists() {
        eprintln!(
            "跳过测试：未找到 wasm 组件 {}，请先执行 `cargo run -p xtask -- build-wasm`",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// 轮询 sidecar 日志（子进程 stderr 全量重定向于此）直至出现标记。
fn wait_log_contains(log: &Path, marker: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(raw) = std::fs::read_to_string(log)
            && raw.contains(marker)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// 引用换代：连接被停止（server 端点变化触发的依赖重启等）后，
/// 适配器静态持有的引用已过期——wasm 内发起的调用应经宿主状态自动
/// 换用注册表现役连接恢复，会话取消同样要打到新连接（打旧引用是
/// no-op，换代后的调用将无法取消）。
///
/// 经真实 memory wasm 组件走完整链路：adapter.handle → wasm 工具 →
/// HostState sidecar host import → stdio 连接；取消是否到达由
/// sidecar 日志中的标记证明。
#[test]
#[serial_test::serial]
fn sidecar连接停止后wasm调用与取消自动换代() {
    use tiangong_core::core::Plugin;

    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let Some(wasm) = memory_wasm_or_skip() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "stale-ref-demo";
    make_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            r#"{"schema_version":2,"id":"stale-ref-demo","version":"0.1.0","entrypoints":["desktop"],"permissions":["sidecar.invoke"],"capabilities":{"prompt":true},"prompt":["占位贡献（本测试经 memory wasm 组件走调用链）"],"sidecar":{"runtime":"node","entry":"sidecar/main.mjs","lifecycle":"resident"}}"#,
        )
        .unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs");
    std::fs::copy(
        &sdk,
        release.join("sidecar/vendor/tiangong-sidecar-sdk/index.mjs"),
    )
    .unwrap();
    // 模拟 memory sidecar 的 recall_context 协议：query 含挂起标记时
    // 永不返回（等取消），cancel 钩子经 stderr 落入 sidecar 日志。
    std::fs::write(
        release.join("sidecar/main.mjs"),
        r#"
import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
await runSidecar({
  pluginId: 'stale-ref-demo',
  pluginVersion: '0.1.0',
  dispatch(operation, payload) {
    if (operation === 'recall_context') {
      const query = String(payload?.request?.query ?? '');
      if (query.includes('__hang__')) {
        console.error('hang-accepted');
        return new Promise(() => {});
      }
      return { payload: { response: { content: `回忆完成：${query}`, hits: [] } } };
    }
    return { payload: {} };
  },
  cancel() {
    console.error('cancel-reached');
  },
});
"#,
    )
    .unwrap();
    write_content_manifest(&release);
    install(root.path(), id, None).expect("插件应可安装");

    // 模拟加载路径：经注册表取连接，注入 wasm 宿主状态与适配器。
    let installed = crate::registry::find_installed_plugin(root.path(), id).unwrap();
    let first =
        crate::registry::sidecar_connection(root.path(), &installed, false).expect("建立初始连接");
    let bytes = std::fs::read(&wasm).unwrap();
    let component = crate::loader::compile_component(&bytes).expect("编译 wasm 组件失败");
    let config = crate::config::PluginRuntimeConfig::default();
    let plugin =
        crate::loader::instantiate_component(&component, &config, Some(first.clone()), id, false)
            .expect("实例化 wasm 组件失败");
    let adapter = crate::adapter::WasmPluginAdapter::new_with_id(
        plugin,
        config,
        true,
        id.to_string(),
        Some(first.clone()),
        "stale-ref-session",
    );
    let runtime = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
    let mut session = tiangong_core::session::Session::new("stale-ref-session");
    session.cwd = root.path().to_string_lossy().into_owned();
    let recall = |query: &str| tiangong_llm::tool::ToolCall {
        id: scru128::new().to_string(),
        name: "recall_memory".into(),
        arguments: serde_json::json!({ "query": query }),
    };

    // 基线：wasm 工具经注入连接完成调用。（memory 组件每轮只回忆一次，
    // 每步先触发轮次开始钩子重置去重标记，确保真实走 sidecar 链路。）
    adapter.on_turn_started();
    let baseline = runtime
        .block_on(
            tiangong_core::tools::extension::ToolOverrideHandler::handle(
                &adapter,
                &recall("基线查询"),
                &mut session,
                "test",
            ),
        )
        .expect("基线调用不得返回 None");
    assert!(baseline.ok, "基线响应: {}", baseline.summary);

    // 模拟 server 端点变化：停连接并清注册表（生产链 stop_connection_for_directory）。
    crate::registry::stop_connection_for_directory(&installed.directory).unwrap();
    assert!(first.is_stopped(), "停止后旧引用应报告已停止");

    // 恢复：同一 wasm 实例再次调用，宿主状态应自动换代连接并成功
    //（无自动恢复时旧引用恒报「已停止」，memory 工具降级为不可用）。
    adapter.on_turn_started();
    let recovered = runtime
        .block_on(
            tiangong_core::tools::extension::ToolOverrideHandler::handle(
                &adapter,
                &recall("恢复后查询"),
                &mut session,
                "test",
            ),
        )
        .expect("恢复调用不得返回 None");
    assert!(
        recovered.ok,
        "停止后 wasm 调用应经换代连接自动恢复: {}",
        recovered.summary
    );

    // 取消：挂起的调用被取消时，取消必须打到换代后的现役连接
    //（打旧引用是 no-op，sidecar 日志不会出现 cancel-reached）。
    let log = installed.directory.join("logs").join("sidecar.log");
    adapter.on_turn_started();
    let future = tiangong_core::tools::extension::ToolOverrideHandler::handle(
        &adapter,
        &recall("挂起 __hang__"),
        &mut session,
        "test",
    );
    let task = runtime.spawn(future);
    assert!(
        wait_log_contains(&log, "hang-accepted", std::time::Duration::from_secs(10)),
        "挂起请求应到达 sidecar"
    );
    task.abort();
    let cancelled = wait_log_contains(&log, "cancel-reached", std::time::Duration::from_secs(10));
    // 兜底清场：断言失败时也要停掉现役连接，让挂起的 wasm 调用返回，
    // runtime 才能正常退出（成功路径下 cancelled 响应已让它返回）。
    let _ = crate::registry::stop_connection_for_directory(&installed.directory);
    drop(task);
    drop(runtime);
    assert!(
        cancelled,
        "取消应打到换代后的现役连接，日志未见 cancel-reached：{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}
/// 自定义图标往返：带 png 图标的插件安装后，read_plugin_icon 返回正确
/// 字节与 MIME（read 走 loaded_plugins 内存表，install 后可用）。
#[test]
#[serial_test::serial]
fn integration_插件图标_安装与读取往返() {
    let root = tempfile::tempdir().unwrap();
    // 纯 UI 产物无 sidecar：安装不触发沙箱内验证，无需原生沙箱环境。
    init_config_with_launcher(root.path());
    let id = "icon-demo";
    make_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::create_dir_all(release.join("icons")).unwrap();
    // 最小 PNG（1x1 透明像素）。
    const MINIMAL_PNG: [u8; 67] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(release.join("icons/app.png"), MINIMAL_PNG).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            r#"{"schema_version":2,"id":"icon-demo","version":"0.1.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","title":"图标验证","entry":"app/index.html","icon":"icons/app.png"}]}}"#,
        )
        .unwrap();
    std::fs::write(release.join("app/index.html"), "<html></html>").unwrap();
    write_content_manifest(&release);

    install(root.path(), id, None).expect("带图标插件安装");

    let (data, mime) = crate::registry::read_plugin_icon(id, "app").expect("读取插件图标");
    assert_eq!(mime, "image/png");
    assert_eq!(data.as_slice(), MINIMAL_PNG);
}
/// 工具级超时与进程终止：阻塞型 node 工具（sleep）超过工具声明的
/// timeout_ms 时及时失败，且按需 sidecar 进程被终止（不留活进程）。
#[test]
#[ignore = "Agent Handler 不再自动超时；仅显式取消终止"]
#[serial_test::serial]
fn integration_直连工具_超时终止进程() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "blocking-tool";
    make_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            r#"{"schema_version":2,"id":"blocking-tool","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide","sidecar.invoke"],"capabilities":{"tools":true},"tools":[{"name":"slow_job","description":"慢任务","input_schema":{"type":"object"},"timeout_ms":30000}],"sidecar":{"runtime":"node","entry":"sidecar/main.mjs","request_timeout_ms":60000}}"#,
        )
        .unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs");
    std::fs::copy(
        &sdk,
        release.join("sidecar/vendor/tiangong-sidecar-sdk/index.mjs"),
    )
    .unwrap();
    std::fs::write(
        release.join("sidecar/main.mjs"),
        r#"
import { writeFileSync } from 'node:fs';
import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
await runSidecar({
  pluginId: 'blocking-tool',
  dispatch(operation, payload) {
    if (operation === 'slow_job') {
      writeFileSync(payload.pid_file, String(process.pid));
      return new Promise(() => {});
    }
    return { payload: {} };
  },
});
"#,
    )
    .unwrap();
    write_content_manifest(&release);

    install(root.path(), id, None).expect("安装阻塞工具插件");

    let pid_file = root.path().join("slow.pid");
    let started = std::time::Instant::now();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            crate::ts_plugin::invoke_sidecar_tool_for_test(
                id,
                tiangong_llm::tool::ToolCall {
                    id: "test-call".to_string(),
                    name: "slow_job".to_string(),
                    arguments: json!({"pid_file": &pid_file}),
                },
                30000,
            )
            .await
        });
    assert!(
        // 沙箱冷启动（Launcher 验签+隔离层+解释器）计入工具预算。
        started.elapsed() < std::time::Duration::from_secs(45),
        "超时应及时返回，实际 {:?}",
        started.elapsed()
    );
    assert!(!result.ok, "超时应失败: {}", result.summary);
    // 进程终止断言：pid 文件出现后，对应进程应在短时间内消失。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let pid = wait_for_pid_file(
        &pid_file,
        std::time::Duration::from_secs(5),
        &format!("慢调用结果：{}", result.summary),
    );
    while std::time::Instant::now() < deadline {
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !alive {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(!alive, "超时后按需 sidecar 进程（pid {pid}）应被终止");
}
/// 并发隔离：同一插件并发直连调用互不影响——两个并发快调用都成功；
/// 一个慢调用超时被终止的同时，并发进行的快调用正常完成；结束后无
/// 遗留进程。
#[test]
#[ignore = "Agent Handler 不再自动超时；并发取消由显式取消链路覆盖"]
#[serial_test::serial]
fn integration_直连工具_并发隔离与取消归属() {
    let Some(_node) = find_node_for_test() else {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    require_native_sandbox!();
    init_config_with_launcher(root.path());
    let id = "concurrent-tool";
    make_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
            release.join("plugin.json"),
            r#"{"schema_version":2,"id":"concurrent-tool","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide","sidecar.invoke"],"capabilities":{"tools":true},"tools":[{"name":"quick_job","description":"快","input_schema":{"type":"object"},"timeout_ms":60000},{"name":"slow_job","description":"慢","input_schema":{"type":"object"},"timeout_ms":30000}],"sidecar":{"runtime":"node","entry":"sidecar/main.mjs","request_timeout_ms":60000}}"#,
        )
        .unwrap();
    let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs");
    std::fs::copy(
        &sdk,
        release.join("sidecar/vendor/tiangong-sidecar-sdk/index.mjs"),
    )
    .unwrap();
    std::fs::write(
            release.join("sidecar/main.mjs"),
            r#"
import { writeFileSync } from 'node:fs';
import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
await runSidecar({
  pluginId: 'concurrent-tool',
  dispatch(operation, payload) {
    if (operation === 'quick_job') {
      return { payload: { ok: true, summary: `完成 ${payload?.tag ?? ''}`, stdout: '', stderr: '', exit_code: 0 } };
    }
    if (operation === 'slow_job') {
      writeFileSync(payload.pid_file, String(process.pid));
      return new Promise(() => {});
    }
    return { payload: {} };
  },
});
"#,
        )
        .unwrap();
    write_content_manifest(&release);
    install(root.path(), id, None).expect("安装并发工具插件");

    let pid_file = root.path().join("slow.pid");
    let call = |name: &str, args: serde_json::Value| tiangong_llm::tool::ToolCall {
        id: format!("call-{name}"),
        name: name.to_string(),
        arguments: args,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::join!(
                async {
                    let result = crate::ts_plugin::invoke_sidecar_tool_for_test(
                        id,
                        call("quick_job", json!({"tag": "并发一"})),
                        60000,
                    )
                    .await;
                    assert!(result.ok, "并发快调用一应成功: {}", result.summary);
                    assert_eq!(result.summary, "完成 并发一");
                },
                async {
                    let result = crate::ts_plugin::invoke_sidecar_tool_for_test(
                        id,
                        call("quick_job", json!({"tag": "并发二"})),
                        60000,
                    )
                    .await;
                    assert!(result.ok, "并发快调用二应成功: {}", result.summary);
                    assert_eq!(result.summary, "完成 并发二");
                },
                async {
                    let result = crate::ts_plugin::invoke_sidecar_tool_for_test(
                        id,
                        call("slow_job", json!({"pid_file": &pid_file})),
                        30000,
                    )
                    .await;
                    assert!(!result.ok, "慢调用应超时失败");
                },
            );
        });

    // 慢调用的按需进程被终止；无遗留。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let pid = wait_for_pid_file(&pid_file, std::time::Duration::from_secs(5), "并发慢调用");
    let process_alive = |pid: i32| {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    while process_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!process_alive(pid), "超时调用的进程（pid {pid}）应被终止");
}
