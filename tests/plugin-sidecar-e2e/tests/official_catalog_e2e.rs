//! 官方目录端到端（显式环境门控）：目录发现 → 归档下载 → 安全解包 →
//! 签名验证 → 安装 → sidecar 真实调用 → 卸载。
//!
//! 信任语义：官方信任根是应用内置公钥、不可配置——构建产物由测试密钥
//! 签署 `publisher=tiangong-official`，必须被安装链拒绝（测试密钥不可
//! 冒充官方）；同一产物改署第三方发布者并导入其公钥后，完整链路放行。
//!
//! 触发条件：`TIANGONG_PLUGIN_E2E_DIST`（发布产物目录，默认 workspace 的
//! `target/plugin-dist`，先经 `cargo run -p xtask -- build-plugin
//! plugin-creator` 生成）；设置 `TIANGONG_PLUGIN_E2E_REQUIRED=1` 后进入
//! fail-closed 模式——缺产物直接失败而非跳过（CI 使用，防止前置断言缺失
//! 时假绿灯）。

use std::path::{Path, PathBuf};

fn workspace_target_dist() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-dist")
}

fn plugin_creator_version() -> String {
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../plugins/tiangong-plugin-creator/plugin.json"
    ))
    .expect("解析 Plugin Creator 清单");
    manifest["version"]
        .as_str()
        .expect("Plugin Creator 清单缺少版本")
        .to_string()
}

fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}

/// 官方目录安装链：本地 http 目录服务承载真实发布产物。产物目录复制到
/// 临时目录并重写 URL——不改写共享构建产物，测试互不污染。
#[test]
#[serial_test::serial]
fn 官方目录_测试密钥冒充官方被拒_三方签名完整链路与卸载() {
    let dist = std::env::var_os("TIANGONG_PLUGIN_E2E_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(workspace_target_dist);
    let version = plugin_creator_version();
    let release_dir = dist.join(format!("plugins/plugin-creator/{version}"));
    let archive = release_dir.join(format!("plugin-creator-{version}.tar.zst"));
    let release_json = release_dir.join("release.json");
    if !archive.is_file() || !release_json.is_file() {
        let reason = format!(
            "缺少发布产物（archive={} release.json={}）",
            archive.display(),
            release_json.display()
        );
        assert!(
            std::env::var_os("TIANGONG_PLUGIN_E2E_REQUIRED").is_none(),
            "E2E fail-closed 模式下不得跳过：{reason}"
        );
        eprintln!("跳过：{reason}（CI 设 TIANGONG_PLUGIN_E2E_REQUIRED=1 强制必跑）");
        return;
    }

    // 产物副本（重写 catalog/fragment 中的 OSS URL 到本地服务）。
    let dist_copy = tempfile::tempdir().unwrap();
    copy_tree(&dist, dist_copy.path());
    let rewrite_oss_urls = |path: &Path, port: u16| {
        let raw = std::fs::read_to_string(path).unwrap();
        std::fs::write(
            path,
            raw.replace(
                "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com",
                &format!("http://127.0.0.1:{port}"),
            ),
        )
        .unwrap();
    };

    // 本地目录服务（随机端口）。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string()])
        .current_dir(dist_copy.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("启动本地目录服务失败");
    std::thread::sleep(std::time::Duration::from_millis(500));
    rewrite_oss_urls(
        &dist_copy
            .path()
            .join("plugins-index/fragments/plugin-creator-any.json"),
        port,
    );
    rewrite_oss_urls(&dist_copy.path().join("plugins-index/catalog.json"), port);

    let storage = tempfile::tempdir().unwrap();
    tiangong_plugin_runtime::test_support::ensure_test_launcher_signed(storage.path())
        .expect("准备 E2E 测试 Launcher");
    tiangong_config::registry::init_from_dir(&storage.path().join("config"));

    let previous_catalog = std::env::var("TIANGONG_PLUGIN_CATALOG_URL").ok();
    unsafe {
        std::env::set_var(
            "TIANGONG_PLUGIN_CATALOG_URL",
            format!("http://127.0.0.1:{port}/plugins-index/catalog.json"),
        );
    }
    let result = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let repository =
                    tiangong_plugin_runtime::artifacts::PluginRepository::new().expect("构造下载器");

                // ── ① 官方形态产物（测试密钥签署 publisher=tiangong-official）：
                //    下载与解包正常，安装链验签必须拒绝——官方信任根是内置
                //    公钥，测试密钥不可冒充官方。
                let staged = repository
                    .download(storage.path(), "plugin-creator", None)
                    .await
                    .expect("目录发现与归档下载解包");
                let error = tiangong_plugin_runtime::registry::install_staged_plugin(
                    storage.path(),
                    staged.path(),
                )
                .expect_err("测试密钥冒充官方形态必须被拒绝");
                assert!(
                    format!("{error:#}").contains("签名验证不通过"),
                    "应报官方验签失败：{error:#}"
                );

                // ── ② 同一产物改署第三方发布者（acme-dev）并以测试密钥重签，
                //    公钥导入测试存储的信任登记表后完整放行。
                let third_party_key =
                    minisign::KeyPair::generate_unencrypted_keypair().expect("生成三方密钥");
                let release_path = dist_copy
                    .path()
                    .join(format!("plugins/plugin-creator/{version}/release.json"));
                let mut release: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&release_path).unwrap()).unwrap();
                release["publisher"] = serde_json::Value::String("acme-dev".to_string());
                let release_raw = serde_json::to_vec_pretty(&release).unwrap();
                std::fs::write(&release_path, &release_raw).unwrap();
                let signature = minisign::sign(
                    Some(&third_party_key.pk),
                    &third_party_key.sk,
                    release_raw.as_slice(),
                    None,
                    None,
                )
                .unwrap();
                use base64::Engine;
                std::fs::write(
                    dist_copy
                        .path()
                        .join(format!("plugins/plugin-creator/{version}/release.json.sig")),
                    base64::engine::general_purpose::STANDARD.encode(signature.into_string()),
                )
                .unwrap();
                let public_b64 = base64::engine::general_purpose::STANDARD
                    .encode(third_party_key.pk.to_box().unwrap().into_string());
                tiangong_plugin_runtime::import_trusted_publisher(
                    storage.path(),
                    "acme-dev",
                    &public_b64,
                )
                .expect("导入三方公钥");

                let staged = repository
                    .download(storage.path(), "plugin-creator", None)
                    .await
                    .expect("三方形态目录发现与下载");
                let status = tiangong_plugin_runtime::registry::install_staged_plugin(
                    storage.path(),
                    staged.path(),
                )
                .expect("三方签名插件安装");
                assert_eq!(status.manifest_version, version);
                tiangong_plugin_runtime::registry::preload_installed_plugins(storage.path());
                let response = tiangong_plugin_runtime::registry::invoke_sidecar(
                    storage.path(),
                    "plugin-creator",
                    "devkit.validate",
                    serde_json::json!({"args": ["nonexistent"], "root": "/tmp/catalog-install-dev"}),
                )
                .unwrap_or_else(|error| {
                    let log = storage
                        .path()
                        .join("plugins/plugin-creator/logs/sidecar.log");
                    let log_tail = std::fs::read_to_string(&log).unwrap_or_else(|read_error| {
                        format!("读取 {} 失败: {read_error}", log.display())
                    });
                    panic!("三方签名插件 sidecar 调用: {error:#}; sidecar 日志:
{log_tail}")
                });
                assert_eq!(
                    response["ok"],
                    serde_json::json!(false),
                    "探针项目应校验失败（证明真实执行）"
                );

                // ── ③ 正规卸载：目录移除 + 注册表清理；重新预加载不自动恢复
                //    （无内置部署通道是结构性保证）。
                tiangong_plugin_runtime::registry::uninstall_plugin(
                    storage.path(),
                    "plugin-creator",
                    false,
                )
                .expect("卸载");
                assert!(
                    !storage
                        .path()
                        .join("plugins/plugin-creator/plugin.json")
                        .is_file(),
                    "卸载后插件目录应移除"
                );
                tiangong_plugin_runtime::registry::preload_installed_plugins(storage.path());
                assert!(
                    tiangong_plugin_runtime::registry::plugin_install_directory("plugin-creator")
                        .is_none(),
                    "卸载后重新预加载不得自动恢复"
                );
            });
    });
    // 恢复现场。
    unsafe {
        match previous_catalog {
            Some(value) => std::env::set_var("TIANGONG_PLUGIN_CATALOG_URL", value),
            None => std::env::remove_var("TIANGONG_PLUGIN_CATALOG_URL"),
        }
    }
    let _ = server.kill();
    let _ = server.wait();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
