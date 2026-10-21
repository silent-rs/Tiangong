//! Plugin Creator 真实产物与宿主的端到端测试：安装 `plugins/tiangong-plugin-creator`
//! 的构建产物（`release/`，由 `yarn package` 生成、不入库），经按需 sidecar
//! 真实执行 devkit 命令。
//!
//! 从 `tiangong-plugin-runtime` 迁出：宿主基础库的测试不读取具体插件目录。
//! 产物缺失或无 node 时跳过。

use std::path::{Path, PathBuf};

use serde_json::json;
use tiangong_plugin_runtime::plugin_dev::PLUGIN_DEV_DIR;
use tiangong_plugin_runtime::test_support;

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
