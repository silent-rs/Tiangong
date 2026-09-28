//! 自动分屏真机验证（仅 macOS）：把「宿主」窗口放到屏幕左侧 400pt、目标
//! 窗口铺满右侧，读回位置后再把宿主窗口恢复原位（目标窗口不动）。
//!
//! 用法（终端需已授予辅助功能权限）：
//! ```sh
//! TIANGONG_PLUGIN_HOST_PID=<天工 pid> cargo run -p tiangong-plugin-computer-use-sidecar \
//!   --example split_demo -- <目标 pid> [停留秒数]
//! ```

#[cfg(target_os = "macos")]
fn main() {
    use std::time::Duration;
    use tiangong_plugin_computer_use_sidecar::backend::{mac_split, overlay};

    let mut args = std::env::args().skip(1);
    let target_pid: i32 = args
        .next()
        .and_then(|v| v.parse().ok())
        .expect("用法：split_demo <目标 pid> [停留秒数]");
    let hold: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(3);

    // 与 sidecar 相同：主线程跑 overlay 循环（NSScreen 查询依赖它）。
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let outcome = mac_split::split_with_host(target_pid).await;
            println!("分屏结果：{outcome:#?}");
            println!(
                "分屏状态：{:?}",
                tiangong_plugin_computer_use_sidecar::split::state()
            );
            tokio::time::sleep(Duration::from_secs(hold)).await;
            match tiangong_plugin_computer_use_sidecar::split::saved_host() {
                Some(saved) => {
                    println!("恢复到：{:?}", saved.bounds);
                    println!("恢复结果：{:?}", mac_split::restore_host(saved).await);
                    tiangong_plugin_computer_use_sidecar::split::clear();
                }
                None => println!("未记录原位置，跳过恢复"),
            }
        });
        overlay::request_shutdown();
    });
    overlay::run_main_loop();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("split_demo 仅支持 macOS");
}
