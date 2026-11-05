//! Plugin Creator 真实产物与宿主的端到端测试：安装 `plugins/tiangong-plugin-creator`
//! 的构建产物（`release/`，由 `yarn package` 生成、不入库），经按需 sidecar
//! 真实执行 devkit 命令。
//!
//! 从 `tiangong-plugin-runtime` 迁出：宿主基础库的测试不读取具体插件目录。
//! 产物缺失或无 node 时跳过。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tiangong_plugin_runtime::plugin_dev::PLUGIN_DEV_DIR;
use tiangong_plugin_runtime::test_support;
use tiangong_plugin_runtime::{InstallerIdentity, set_plugin_dev_trusted_installer};

const CREATOR_ID: &str = "plugin-creator";

fn creator_release() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/tiangong-plugin-creator/release")
}

fn find_node() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TIANGONG_NODE_PATH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "node.exe" } else { "node" }))
        .find(|path| path.is_file())
}

fn copy_tree(source: &Path, target: &Path) {
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

/// 前置条件齐备时返回已安装 creator 的临时存储根；否则打印原因并返回 None。
fn prepare_creator() -> Option<tempfile::TempDir> {
    if find_node().is_none() {
        eprintln!("跳过：PATH 中未找到 node");
        return None;
    }
    let release = creator_release();
    if !release.join("plugin.json").is_file() {
        eprintln!(
            "跳过：缺少真实构建产物 {}（先在 creator 目录 yarn package）",
            release.display()
        );
        return None;
    }
    if let Some(reason) = test_support::native_sandbox_skip_reason() {
        eprintln!("跳过真实沙箱测试：当前环境无法应用原生沙箱：{reason}");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    // 安装链的可用性探测读取全局模型配置；测试进程用隔离目录初始化。
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    // 宿主权威策略表与生产同源（tiangong-plugin-policy）：创作链需要
    // 插件开发目录写域。runtime 不含插件 ID，未注入即全沙箱零特权。
    tiangong_plugin_policy::install();
    test_support::ensure_test_launcher_signed(root.path()).unwrap();
    let dev_root = test_support::write_dev_project(root.path(), CREATOR_ID);
    copy_tree(&release, &dev_root.join("release"));
    let installed =
        test_support::install_dev_project(root.path(), CREATOR_ID).expect("creator 真实产物安装");
    assert_eq!(installed, CREATOR_ID);
    Some(root)
}

/// 真实 creator 产物全链路：plugin-dev 安装（暂存 + 双向完整性 + 本地
/// 签名）→ 按需 sidecar 真实执行 devkit.init（验证 templates 随行资源经
/// resources 声明进入安装目录并被 devkit 使用）。
#[test]
#[serial_test::serial]
fn integration_creator真实产物_安装与devkit_init全链路() {
    let Some(root) = prepare_creator() else {
        return;
    };
    let init_root = root.path().join("init-output");
    let response = tiangong_plugin_runtime::registry::invoke_sidecar(
        root.path(),
        CREATOR_ID,
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

/// 完整用户旅程（显式运行：`cargo test -p tiangong-plugin-sidecar-e2e --test
/// plugin_creator_e2e -- --ignored`）：经 creator 的按需 sidecar 从零创建
/// node-sidecar 插件 → 注入自定义操作 → devkit 真实构建（yarn 工程链）→
/// 安装 → 宿主连接新插件的 sidecar 真实调用。全程不经 GUI，等价于 Agent
/// 操作序列。
#[test]
#[ignore = "真实 yarn 构建需数分钟与网络，按需显式运行"]
#[serial_test::serial]
fn integration_从零创建node_sidecar插件_完整旅程() {
    let Some(root) = prepare_creator() else {
        return;
    };
    let dev_root = root.path().join(PLUGIN_DEV_DIR);
    let devkit = |command: &str, args: serde_json::Value| {
        tiangong_plugin_runtime::registry::invoke_sidecar(
            root.path(),
            CREATOR_ID,
            &format!("devkit.{command}"),
            args,
        )
    };
    // 1) 初始化 node-sidecar 模板项目（真实模板，写入受控开发根）。
    let project_id = "journey-demo";
    let response = devkit(
        "init",
        json!({"args": ["node-sidecar", project_id, "--name", "旅程验证"], "root": dev_root}),
    )
    .expect("devkit.init");
    assert_eq!(response["ok"], json!(true), "init 失败: {response}");
    // 2) 修改 sidecar 源码：注入自定义操作（模拟用户/Agent 编码）。
    let entry = dev_root.join(project_id).join("sidecar/main.mjs");
    let source = std::fs::read_to_string(&entry).unwrap();
    let patched = source.replace(
        "    if (operation === 'demo.echo') {",
        "    if (operation === 'journey.greet') {\n      return { payload: { message: `你好，${payload?.who ?? '天工'}！` } };\n    }\n    if (operation === 'demo.echo') {",
    );
    assert!(patched != source, "应成功注入自定义操作");
    std::fs::write(&entry, patched).unwrap();
    // 3) 真实构建（yarn install + 类型检查 + vite/esbuild 双端打包）。
    let response =
        devkit("build", json!({"args": [project_id], "root": dev_root})).expect("devkit.build");
    assert_eq!(response["ok"], json!(true), "build 失败: {response}");
    // 4) 安装新插件（完整性 + 本地签名）。
    let installed = test_support::install_dev_project(root.path(), project_id).expect("新插件安装");
    assert_eq!(installed, project_id);
    // 5) 宿主连接新插件 sidecar，真实调用自定义操作。
    let response = tiangong_plugin_runtime::registry::invoke_sidecar(
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

// ── plugin-dev 宿主通道功能测试（自 runtime crate 迁入）──

/// 安装一个已加载的轻量调用方（纯 UI 形态，无需 node），供经桥接
/// `plugin-dev.*` 调用的测试使用。
fn install_ui_caller(root: &Path, id: &str) {
    test_support::make_dev_project(root, id);
    let release = root.join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::write(
        release.join("plugin.json"),
        format!(
            r#"{{"schema_version":2,"id":"{id}","version":"0.1.0","permissions":[],"ui":{{"contributions":[{{"slot":"extension.tab","id":"app","entry":"app/index.html"}}]}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(release.join("app/index.html"), "<html></html>").unwrap();
    test_support::write_content_manifest(&release);
    test_support::install_dev_project(root, id).expect("调用方插件安装");
}

fn dev_call(caller: &str, operation: &str, id: &str) -> Result<serde_json::Value, String> {
    tiangong_plugin_runtime::plugin_dev::call(caller, operation, &json!({"id": id}).to_string())
        .map(|raw| serde_json::from_str(&raw).unwrap())
        .map_err(|error| format!("{error:#}"))
}

/// 项目/插件 ID 白名单（防路径逃逸）：非法 ID 在安装入口即被拒绝。
#[test]
#[serial_test::serial]
fn 项目id白名单拒绝路径逃逸() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "空 格"] {
        let error =
            test_support::install_dev_project(root.path(), bad).expect_err("非法插件 ID 必须拒绝");
        assert!(
            format!("{error:#}").contains("插件 ID"),
            "{bad:?} -> {error:#}"
        );
    }
    // 合法 ID（含 - _ .）可走完整装置链。
    test_support::make_dev_project(root.path(), "my-plugin_1.0");
    let release = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join("my-plugin_1.0")
        .join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::write(
        release.join("plugin.json"),
        r#"{"schema_version":2,"id":"my-plugin_1.0","version":"0.1.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}}"#,
    )
    .unwrap();
    std::fs::write(release.join("app/index.html"), "<html></html>").unwrap();
    test_support::write_content_manifest(&release);
    test_support::install_dev_project(root.path(), "my-plugin_1.0").expect("合法 ID 应可安装");
}

#[test]
#[serial_test::serial]
fn list_返回项目与版本状态() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    install_ui_caller(root.path(), "list-caller");
    test_support::make_dev_project(root.path(), "demo");
    let release = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join("demo")
        .join("release");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(
        release.join("plugin.json"),
        r#"{"schema_version":2,"id":"demo","version":"0.1.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}}"#,
    )
    .unwrap();
    let entries = dev_call("list-caller", "plugin-dev.list", "demo");
    let entries = entries.expect("list 桥接调用").as_array().unwrap().clone();
    let demo = entries
        .iter()
        .find(|entry| entry["id"] == json!("demo"))
        .expect("demo 项目应在列表中");
    assert_eq!(demo["source_version"], json!("0.2.0"));
    assert_eq!(demo["release_version"], json!("0.1.0"));
    assert_eq!(demo["installed_version"], serde_json::Value::Null);
}

#[test]
#[serial_test::serial]
fn status_未初始化返回不存在() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    install_ui_caller(root.path(), "status-caller");
    let result = dev_call("status-caller", "plugin-dev.status", "ghost").expect("status 桥接调用");
    assert_eq!(result["exists"], json!(false));
}

/// 安装完整链（功能验证）：暂存（不可变副本）→ 用户密钥签名 → 导入 →
/// 安装目录与注册表就位。无 sidecar 的纯 UI/工具模板同样获得 local 签名
/// （动态调用通道的分流判据，P0 回归）。
#[test]
#[serial_test::serial]
fn install_完整链_暂存确认导入与注册表() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    let project = test_support::make_dev_project(root.path(), "inst-demo");
    let release = project.join("release");
    std::fs::create_dir_all(release.join("dist")).unwrap();
    std::fs::write(
        project.join(".plugin-dev.json"),
        r#"{"plugin_id":"inst-demo","name":"安装链验证","template":"ts-npx","created_at":"t"}"#,
    )
    .unwrap();
    std::fs::write(
        release.join("plugin.json"),
        r#"{"schema_version":2,"id":"inst-demo","version":"9.9.9","permissions":[],"entrypoints":["desktop"],"capabilities":{"prompt":true},"prompt":["能力说明"],"ui":{"contributions":[{"slot":"extension.tab","id":"app","title":"安装链验证","entry":"dist/index.html"}]}}"#,
    )
    .unwrap();
    std::fs::write(release.join("dist/index.html"), "<html></html>").unwrap();
    test_support::write_content_manifest(&release);

    let installed =
        test_support::install_dev_project(root.path(), "inst-demo").expect("安装完整链");
    assert_eq!(installed, "inst-demo");
    assert!(
        root.path().join("plugins/inst-demo/plugin.json").is_file(),
        "安装目录就位"
    );
    assert!(
        root.path()
            .join("plugins/inst-demo/dist/index.html")
            .is_file()
    );
    assert!(
        root.path().join("plugins/inst-demo/release.json").is_file(),
        "纯 UI/工具模板安装后应有签名清单"
    );
    assert!(
        tiangong_plugin_runtime::registry::is_local_plugin("inst-demo"),
        "无 sidecar 自制插件应命中 local 判据"
    );
    assert!(
        tiangong_plugin_runtime::registry::plugin_manifest("inst-demo").is_some(),
        "注册表应可见已装插件"
    );
}

#[test]
#[serial_test::serial]
fn install_缺构建产物时给出devkit指引() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    test_support::make_dev_project(root.path(), "demo");
    let error =
        test_support::install_dev_project(root.path(), "demo").expect_err("缺构建产物应报错");
    assert!(
        format!("{error:#}").contains("构建"),
        "应给出构建指引：{error:#}"
    );
}

/// 纯 UI 产物同样受构建指纹核验约束（失配拒绝、匹配放行）。
#[test]
#[serial_test::serial]
fn integration_纯ui插件_构建指纹核验() {
    let root = tempfile::tempdir().unwrap();
    test_support::init_config_with_launcher(root.path());
    let id = "ui-fp-demo";
    test_support::make_dev_project(root.path(), id);
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
    test_support::write_content_manifest(&release);

    // 失配指纹断言拒绝路径（与桥接 call 层同款核验分支）。
    let mismatch = "0".repeat(64);
    let error =
        test_support::install_dev_project_checked(root.path(), id, "plugin-creator", &mismatch)
            .expect_err("纯 UI 产物同样受指纹核验约束");
    assert!(format!("{error:#}").contains("不一致"), "{error:#}");
    // 正确指纹放行。
    let fingerprint = tiangong_plugin_runtime::content_manifest_fingerprint(&release).unwrap();
    let installed =
        test_support::install_dev_project_checked(root.path(), id, "plugin-creator", &fingerprint)
            .expect("纯 UI 指纹匹配应安装成功");
    assert_eq!(installed, id);
}

/// 自定义图标往返：带 png 图标的插件安装后，read_plugin_icon 返回正确
/// 字节与 MIME（read 走 loaded_plugins 内存表，install 后可用）。
#[test]
#[serial_test::serial]
fn integration_插件图标_安装与读取往返() {
    let root = tempfile::tempdir().unwrap();
    test_support::init_config_with_launcher(root.path());
    let id = "icon-demo";
    test_support::make_dev_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("app")).unwrap();
    std::fs::create_dir_all(release.join("icons")).unwrap();
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
    test_support::write_content_manifest(&release);

    test_support::install_dev_project(root.path(), id).expect("带图标插件安装");
    let (data, mime) =
        tiangong_plugin_runtime::registry::read_plugin_icon(id, "app").expect("读取插件图标");
    assert_eq!(mime, "image/png");
    assert_eq!(data.as_slice(), MINIMAL_PNG);
}

fn require_node_and_sandbox(root: &Path) -> bool {
    if test_support::find_node().is_none() {
        eprintln!("跳过：PATH 中未找到 node");
        return false;
    }
    if test_support::native_sandbox_skip_reason().is_some() {
        eprintln!("跳过真实沙箱测试：宿主在受限沙箱内（Launcher 无法嵌套应用）");
        return false;
    }
    test_support::init_config_with_launcher(root);
    true
}

#[test]
#[serial_test::serial]
fn integration_解释器sidecar_创作链自动签名安装_真实调用与篡改拒绝() {
    let root = tempfile::tempdir().unwrap();
    if !require_node_and_sandbox(root.path()) {
        return;
    }
    let id = "node-sc-demo";
    test_support::make_dev_project(root.path(), id);
    test_support::make_node_sidecar_release(root.path(), id);

    let installed =
        test_support::install_dev_project(root.path(), id).expect("解释器 sidecar 安装");
    assert_eq!(installed, id);

    let response = tiangong_plugin_runtime::registry::invoke_sidecar(
        root.path(),
        id,
        "demo.echo",
        json!({"text": "本地信任"}),
    )
    .expect("sidecar 调用");
    assert_eq!(response["text"], "本地信任");

    // 篡改安装目录内的入口脚本：内容清单复核必须拒绝。
    let installed_entry = root
        .path()
        .join("plugins")
        .join(id)
        .join("sidecar/main.mjs");
    std::fs::write(&installed_entry, "// tampered\n").unwrap();
    let error =
        tiangong_plugin_runtime::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({}))
            .expect_err("篡改后应拒绝启动");
    assert!(
        error.to_string().contains("篡改"),
        "应报篡改错误：{error:#}"
    );
}

/// 安装授权五场景：fail-closed、非受信插件、缺构建登记、受信齐全。
/// 调用方插件为已安装的轻量 UI 插件（桥接只要求调用方已加载）。
#[test]
#[serial_test::serial]
fn integration_安装授权_五场景() {
    let root = tempfile::tempdir().unwrap();
    if !require_node_and_sandbox(root.path()) {
        return;
    }
    let caller = "auth-caller";
    install_ui_caller(root.path(), caller);
    // 待安装的项目产物。
    let target = "auth-target";
    test_support::make_dev_project(root.path(), target);
    test_support::make_node_sidecar_release(root.path(), target);

    // 1) 未注入受信判定：fail-closed。
    test_support::clear_plugin_dev_trusted_installer();
    let error =
        dev_call(caller, "plugin-dev.install", target).expect_err("未注入受信判定应 fail-closed");
    assert!(error.contains("fail-closed"), "{error}");

    // 2) 非受信插件（判定只认别的插件）。
    set_plugin_dev_trusted_installer(Arc::new(|identity: &InstallerIdentity| {
        identity.plugin_id == "someone-else"
    }));
    let error = dev_call(caller, "plugin-dev.install", target).expect_err("非受信插件应拒绝");
    assert!(error.contains("无自动签名安装资格"), "{error}");

    // 3) 受信但缺构建登记。
    let expected_caller = caller.to_string();
    set_plugin_dev_trusted_installer(Arc::new(move |identity: &InstallerIdentity| {
        identity.plugin_id == expected_caller
    }));
    let error = dev_call(caller, "plugin-dev.install", target).expect_err("缺构建登记应拒绝");
    assert!(error.contains("缺少受信构建登记"), "{error}");

    // 4) 受信 + 已登记（指纹锚定构建产物）：完整签名安装链成功。
    let release_dir = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join(target)
        .join("release");
    let fingerprint = tiangong_plugin_runtime::content_manifest_fingerprint(&release_dir).unwrap();
    tiangong_plugin_runtime::note_trusted_build(caller, target, Some(fingerprint));
    let result = dev_call(caller, "plugin-dev.install", target).expect("受信且已登记应安装成功");
    assert_eq!(result["plugin_id"], json!(target));
    test_support::clear_plugin_dev_trusted_installer();
}

/// 指纹核验：构建登记后篡改产物（重算内容清单）再经桥接安装必须拒绝；
/// 成功安装消费登记（再次安装需重新构建）。
#[test]
#[serial_test::serial]
fn integration_指纹核验_篡改产物拒绝与安装消费() {
    let root = tempfile::tempdir().unwrap();
    if !require_node_and_sandbox(root.path()) {
        return;
    }
    let caller = "fp-caller";
    let target = "fp-target";
    install_ui_caller(root.path(), caller);
    test_support::make_dev_project(root.path(), target);
    test_support::make_node_sidecar_release(root.path(), target);

    let expected_caller = caller.to_string();
    set_plugin_dev_trusted_installer(Arc::new(move |identity: &InstallerIdentity| {
        identity.plugin_id == expected_caller
    }));
    let release_dir = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join(target)
        .join("release");
    let fingerprint = tiangong_plugin_runtime::content_manifest_fingerprint(&release_dir).unwrap();
    tiangong_plugin_runtime::note_trusted_build(caller, target, Some(fingerprint));

    // 构建后篡改产物（改文件并重算内容清单）：指纹失配拒绝。
    std::fs::write(
        release_dir.join("sidecar/main.mjs"),
        "// tampered after build",
    )
    .unwrap();
    test_support::write_content_manifest(&release_dir);
    let error = dev_call(caller, "plugin-dev.install", target).expect_err("篡改产物应拒绝安装");
    assert!(error.contains("不一致"), "{error}");

    // 恢复产物并重新登记（等价重新构建）：安装成功且登记被消费。
    test_support::make_node_sidecar_release(root.path(), target);
    let fingerprint = tiangong_plugin_runtime::content_manifest_fingerprint(&release_dir).unwrap();
    tiangong_plugin_runtime::note_trusted_build(caller, target, Some(fingerprint));
    dev_call(caller, "plugin-dev.install", target).expect("登记匹配应安装成功");
    assert!(
        tiangong_plugin_runtime::trusted_build_fingerprint(caller, target).is_none(),
        "安装成功后登记应被消费"
    );
    // 消费后再次安装：要求重新构建。
    let error = dev_call(caller, "plugin-dev.install", target).expect_err("消费后应要求重新构建");
    assert!(error.contains("缺少受信构建登记"), "{error}");
    test_support::clear_plugin_dev_trusted_installer();
}

/// 失败构建撤销：note(None) 语义清除旧登记后不可安装。
#[test]
#[serial_test::serial]
fn integration_失败构建_撤销登记后不可安装() {
    let root = tempfile::tempdir().unwrap();
    if !require_node_and_sandbox(root.path()) {
        return;
    }
    let caller = "revoke-caller";
    let target = "revoke-target";
    install_ui_caller(root.path(), caller);
    test_support::make_dev_project(root.path(), target);
    test_support::make_node_sidecar_release(root.path(), target);

    let expected_caller = caller.to_string();
    set_plugin_dev_trusted_installer(Arc::new(move |identity: &InstallerIdentity| {
        identity.plugin_id == expected_caller
    }));
    let release_dir = root
        .path()
        .join(PLUGIN_DEV_DIR)
        .join(target)
        .join("release");
    let fingerprint = tiangong_plugin_runtime::content_manifest_fingerprint(&release_dir).unwrap();
    tiangong_plugin_runtime::note_trusted_build(caller, target, Some(fingerprint));
    // 构建失败：观察者撤销登记。
    tiangong_plugin_runtime::note_trusted_build(caller, target, None);
    let error = dev_call(caller, "plugin-dev.install", target).expect_err("撤销后应拒绝安装");
    assert!(error.contains("缺少受信构建登记"), "{error}");
    test_support::clear_plugin_dev_trusted_installer();
}

/// 无界面纯工具插件（node-tool 形态）：无 ui 贡献即可安装（校验解耦），
/// 工具契约（操作名=工具名、ToolOutcome 形状）经 sidecar 真实往返。
#[test]
#[serial_test::serial]
fn integration_无界面纯工具插件_安装与工具契约() {
    let root = tempfile::tempdir().unwrap();
    if !require_node_and_sandbox(root.path()) {
        return;
    }
    let id = "pure-tool-demo";
    test_support::make_dev_project(root.path(), id);
    let release = root.path().join(PLUGIN_DEV_DIR).join(id).join("release");
    std::fs::create_dir_all(release.join("sidecar/vendor/tiangong-sidecar-sdk")).unwrap();
    std::fs::write(
        release.join("plugin.json"),
        r#"{"schema_version":2,"id":"pure-tool-demo","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide","sidecar.invoke"],"capabilities":{"tools":true},"tools":[{"name":"text_analyze","description":"文本分析","input_schema":{"type":"object"},"timeout_ms":20000}],"sidecar":{"runtime":"node","entry":"sidecar/main.mjs"},"mention":{"candidates_op":"pure-tool-demo.mention_candidates"}}"#,
    )
    .unwrap();
    let sdk = test_support::node_sidecar_sdk();
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
    test_support::write_content_manifest(&release);

    let installed =
        test_support::install_dev_project(root.path(), id).expect("无界面纯工具插件应可安装");
    assert_eq!(installed, id);

    let response = tiangong_plugin_runtime::registry::invoke_sidecar(
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

/// 三方导入：以三方密钥签发签名清单并打 tar.zst 归档，公钥导入前拒装、
/// 导入后安装与真实调用成功、移除公钥后失效。
#[test]
#[serial_test::serial]
fn integration_三方导入_签名归档安装与移除公钥后失效() {
    let root = tempfile::tempdir().unwrap();
    if !require_node_and_sandbox(root.path()) {
        return;
    }
    let id = "node-sc-third-party";
    test_support::make_dev_project(root.path(), id);
    let release = test_support::make_node_sidecar_release(root.path(), id);

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
    let staged = tiangong_plugin_runtime::artifacts::stage_plugin_archive(root.path(), &archive)
        .expect("归档暂存");
    let error = tiangong_plugin_runtime::registry::import_staged_plugin(root.path(), staged.path())
        .expect_err("未导入三方公钥应拒绝安装");
    assert!(
        error.to_string().contains("未导入"),
        "应给出公钥导入指引：{error:#}"
    );
    drop(staged);

    // 导入公钥后：安装 → 真实调用成功。
    let public_b64 = base64::engine::general_purpose::STANDARD
        .encode(keypair.pk.to_box().unwrap().into_string());
    tiangong_plugin_runtime::trust::import_trusted_publisher(root.path(), "acme-dev", &public_b64)
        .unwrap();
    let staged = tiangong_plugin_runtime::artifacts::stage_plugin_archive(root.path(), &archive)
        .expect("归档暂存");
    tiangong_plugin_runtime::registry::import_staged_plugin(root.path(), staged.path())
        .expect("三方签名插件安装");
    let response = tiangong_plugin_runtime::registry::invoke_sidecar(
        root.path(),
        id,
        "demo.echo",
        json!({"text": "三方"}),
    )
    .expect("三方插件 sidecar 调用");
    assert_eq!(response["text"], "三方");
    // 安装成功后事务目录不应累积残留。
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
    tiangong_plugin_runtime::trust::remove_trusted_publisher(root.path(), "acme-dev").unwrap();
    let error =
        tiangong_plugin_runtime::registry::invoke_sidecar(root.path(), id, "demo.echo", json!({}))
            .expect_err("移除公钥后应拒绝启动");
    assert!(
        error.to_string().contains("未导入"),
        "应报公钥缺失：{error:#}"
    );
}
