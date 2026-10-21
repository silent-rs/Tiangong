//! 测试副本同步护栏：runtime 测试使用的 Node sidecar SDK 副本
//! （`tests/fixtures/node-sidecar-sdk/index.mjs`）必须与真实 SDK
//! （`plugins/sdk-sidecar/index.mjs`）逐字节一致，避免 runtime 测试在过期
//! 协议上通过。

use std::path::Path;

#[test]
fn node_sidecar_sdk_测试副本与真实_sdk_一致() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = std::fs::read(root.join("tests/fixtures/node-sidecar-sdk/index.mjs"))
        .expect("读取 SDK 测试副本失败");
    let source =
        std::fs::read(root.join("plugins/sdk-sidecar/index.mjs")).expect("读取真实 SDK 失败");
    assert!(
        fixture == source,
        "tests/fixtures/node-sidecar-sdk/index.mjs 与 plugins/sdk-sidecar/index.mjs 不一致，\
         执行 cp plugins/sdk-sidecar/index.mjs tests/fixtures/node-sidecar-sdk/index.mjs 同步"
    );
}
