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
    crate::test_support::native_sandbox_skip_reason()
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
    let sdk = crate::test_support::node_sidecar_sdk();
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
    let sdk = crate::test_support::node_sidecar_sdk();
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
    let sdk = crate::test_support::node_sidecar_sdk();
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
    let sdk = crate::test_support::node_sidecar_sdk();
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
