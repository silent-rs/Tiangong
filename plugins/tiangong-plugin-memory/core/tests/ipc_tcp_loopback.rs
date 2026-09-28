use serial_test::serial;
use tempfile::TempDir;
use tiangong_memory::ipc::protocol::{IpcRequest, IpcResponse};
use tiangong_memory::ipc::spawn_memory_bridge;
use tiangong_memory::ipc::{IpcClient, IpcServer, load_endpoint};
use tiangong_memory::{Episode, EpisodeOutcome, MemoryHandle, RecallAnchors, start};

struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_userprofile: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn enter(home: &std::path::Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
        }
        Self {
            prev_home,
            prev_userprofile,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn public_tcp_loopback_ipc_api_roundtrip_works() {
    let home = TempDir::new().expect("创建 fake home 失败");
    let _env = EnvGuard::enter(home.path());

    let server = IpcServer::bind("memory-public-ipc")
        .await
        .expect("绑定 IPC 失败");
    let endpoint = load_endpoint("memory-public-ipc").expect("读取 endpoint 失败");

    let server_task = tokio::spawn(async move {
        let mut conn = server
            .accept_authenticated()
            .await
            .expect("服务端接受连接失败");
        let req = conn.read_request().await.expect("服务端读取请求失败");
        conn.write_response(IpcResponse {
            request_id: req.request_id,
            payload: serde_json::json!({ "pong": true }),
        })
        .await
        .expect("服务端写响应失败");
    });

    let mut client = IpcClient::connect_endpoint(&endpoint)
        .await
        .expect("客户端连接失败");
    client
        .send_request(IpcRequest {
            request_id: "ping-1".to_string(),
            payload: serde_json::json!({ "ping": true }),
        })
        .await
        .expect("客户端写请求失败");
    let response = client.read_response().await.expect("客户端读响应失败");

    assert_eq!(response.request_id, "ping-1");
    assert_eq!(response.payload["pong"], true);

    server_task.await.expect("服务端任务失败");
}

async fn wait_for_remote_recall_hit(
    handle: &MemoryHandle,
    query: &str,
) -> Vec<tiangong_memory::RecallHit> {
    for _ in 0..20 {
        let hits = handle
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                5,
            )
            .await;
        if !hits.is_empty() {
            return hits;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Vec::new()
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn remote_memory_handle_can_write_and_recall_via_tcp_bridge() {
    let home = TempDir::new().expect("创建 fake home 失败");
    let _env = EnvGuard::enter(home.path());

    let local_handle = start().expect("启动本地 memory 失败");
    let _bridge = spawn_memory_bridge("memory-remote-handle", local_handle.clone())
        .expect("启动 memory bridge 失败");
    let remote_handle = MemoryHandle::connect_tcp("memory-remote-handle")
        .await
        .expect("连接远端 memory 失败");

    remote_handle.write_episode(
        Episode::new(
            "session-remote".to_string(),
            "repair tcp bridge".to_string(),
            "repair tcp bridge recall flow".to_string(),
            EpisodeOutcome::Success,
            vec![
                "tcp".to_string(),
                "bridge".to_string(),
                "recall".to_string(),
            ],
            vec!["memory_ipc".to_string()],
            0.9,
        ),
        None,
    );

    let hits = wait_for_remote_recall_hit(&remote_handle, "tcp bridge").await;
    assert!(
        hits.iter().any(|hit| hit.title.contains("tcp bridge")),
        "远端句柄写入的 episode 应能被远端 recall 命中"
    );

    remote_handle.shutdown().await;
}
