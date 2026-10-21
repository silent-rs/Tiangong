//! 官方插件签名和制品完整性边界测试。

use std::path::{Path, PathBuf};

use tiangong_plugin_runtime::artifacts::stage_local_plugin;
use tiangong_plugin_runtime::manifest::PluginManifest;
use tiangong_plugin_runtime::signature::verify_signed_release;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signed-plugin")
}

fn copy_fixture() -> PathBuf {
    let target = std::env::temp_dir().join(format!("tiangong-signature-test-{}", scru128::new()));
    copy_dir(&fixture_dir(), &target);
    target
}

fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("创建测试目录失败");
    for entry in std::fs::read_dir(source).expect("读取签名测试制品失败") {
        let entry = entry.expect("读取测试制品条目失败");
        std::fs::copy(entry.path(), target.join(entry.file_name())).expect("复制测试制品失败");
    }
}

fn verify(
    directory: &Path,
) -> anyhow::Result<Option<tiangong_plugin_runtime::signature::SignedPluginRelease>> {
    let manifest = PluginManifest::load(&directory.join("plugin.json"))?;
    verify_signed_release(directory, &manifest)
}

fn tamper(path: &Path) {
    let mut bytes = std::fs::read(path).expect("读取待篡改制品失败");
    bytes[0] ^= 1;
    std::fs::write(path, bytes).expect("写入篡改制品失败");
}

fn tamper_signature(path: &Path) {
    let encoded = std::fs::read_to_string(path).expect("读取待篡改签名失败");
    let mut decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
            .expect("解码待篡改签名失败");
    let index = decoded.len() / 2;
    decoded[index] ^= 1;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, decoded);
    std::fs::write(path, encoded).expect("写入篡改签名失败");
}

#[test]
fn valid_official_release_is_accepted() {
    let directory = fixture_dir();
    let release = verify(&directory)
        .expect("官方签名制品应通过验证")
        .expect("应识别为签名发布");
    assert!(release.has_permission("sidecar.invoke"));
    assert!(release.has_permission("model-config.read"));
}

#[test]
fn unsigned_plugin_is_preserved_as_unsigned() {
    let directory = copy_fixture();
    std::fs::remove_file(directory.join("release.json")).unwrap();
    std::fs::remove_file(directory.join("release.json.sig")).unwrap();
    assert!(
        verify(&directory)
            .expect("未签名纯 WASM 的签名探测不应失败")
            .is_none()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn incomplete_signature_pair_is_rejected() {
    for missing in ["release.json", "release.json.sig"] {
        let directory = copy_fixture();
        std::fs::remove_file(directory.join(missing)).unwrap();
        assert!(verify(&directory).is_err(), "缺少 {missing} 时必须拒绝");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn tampered_artifacts_are_rejected() {
    for artifact in ["plugin.json", "fixture.wasm", "fixture-sidecar"] {
        let directory = copy_fixture();
        tamper(&directory.join(artifact));
        assert!(verify(&directory).is_err(), "篡改 {artifact} 后必须拒绝");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn tampered_release_and_signature_are_rejected() {
    for artifact in ["release.json", "release.json.sig"] {
        let directory = copy_fixture();
        if artifact == "release.json.sig" {
            tamper_signature(&directory.join(artifact));
        } else {
            tamper(&directory.join(artifact));
        }
        assert!(verify(&directory).is_err(), "篡改 {artifact} 后必须拒绝");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn local_staging_copies_signature_pair() {
    let storage_root = std::env::temp_dir().join(format!("tiangong-stage-test-{}", scru128::new()));
    let staged = stage_local_plugin(&storage_root, &fixture_dir()).expect("签名插件暂存失败");
    assert!(staged.path().join("release.json").is_file());
    assert!(staged.path().join("release.json.sig").is_file());
    assert!(verify(staged.path()).expect("暂存制品签名应有效").is_some());
    drop(staged);
    std::fs::remove_dir_all(storage_root).unwrap();
}

#[test]
fn local_staging_preserves_incomplete_pair_for_uniform_rejection() {
    let source = copy_fixture();
    std::fs::remove_file(source.join("release.json.sig")).unwrap();
    let storage_root = std::env::temp_dir().join(format!("tiangong-stage-test-{}", scru128::new()));
    let staged = stage_local_plugin(&storage_root, &source).expect("暂存阶段只负责复制现存制品");
    assert!(
        verify(staged.path()).is_err(),
        "安装前统一验证必须拒绝残缺签名"
    );
    drop(staged);
    std::fs::remove_dir_all(source).unwrap();
    std::fs::remove_dir_all(storage_root).unwrap();
}
