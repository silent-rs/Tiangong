//! stdio 传输 server（RFC 0017 D16 / S2）：与宿主
//! `tiangong_plugin_runtime::sidecar::StdioSidecarConnection` 对接。
//!
//! 帧协议与 TCP 完全一致；区别仅在通道：stdin 读帧（Auth 首帧校验 token、
//! Request 交业务 dispatch），Response/Notification 写 stdout。stdin EOF
//! 即宿主关闭，进程随之退出——宿主管理生命周期，跳过单例与信号等待。

use std::collections::HashMap;
use std::sync::Arc;

use crate::protocol::{
    ErrorCode as PluginErrorCode, IpcFrame, IpcResponse, Request as PluginRequest,
    RequestInvocationContext, Response as PluginResponse,
};
use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(target_os = "macos")]
use crate::env::HOST_PID_ENV;
use crate::env::{STDIO_TOKEN_ENV, TRANSPORT_ENV, TRANSPORT_STDIO};
use crate::singleton::SidecarService;

const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 16;

tokio::task_local! {
    static REQUEST_PROGRESS: ProgressHandle;
    static REQUEST_CONTEXT: Option<RequestInvocationContext>;
}

#[derive(Clone)]
struct ProgressHandle {
    writer: Arc<tokio::sync::Mutex<tokio::io::Stdout>>,
    request_id: String,
}

pub(crate) async fn emit_progress(message: String) {
    let Ok(handle) = REQUEST_PROGRESS.try_with(Clone::clone) else {
        return;
    };
    let _ = write_frame(
        &handle.writer,
        &IpcFrame::Progress {
            request_id: handle.request_id,
            message,
        },
    )
    .await;
}

pub(crate) fn invocation_context() -> Option<RequestInvocationContext> {
    REQUEST_CONTEXT.try_with(Clone::clone).ok().flatten()
}

/// 宿主是否要求以 stdio 模式运行。
pub fn stdio_requested() -> bool {
    std::env::var(TRANSPORT_ENV).ok().as_deref() == Some(TRANSPORT_STDIO)
}

/// stdio 模式主循环：认证 → 循环分发请求，stdin EOF（宿主关闭）即退出。
pub async fn run_stdio<F>(service_factory: F) -> Result<()>
where
    F: FnOnce() -> Result<Arc<dyn SidecarService>>,
{
    let service_name =
        std::env::var("TIANGONG_PLUGIN_ID").unwrap_or_else(|_| "sidecar".to_string());
    start_host_process_monitor()?;
    let service_obj = service_factory()?;
    tracing::info!(service = %service_name, "stdio sidecar 开始服务");

    // 通知写出任务：订阅全局广播，与响应共用 stdout 写锁。
    let writer = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    {
        let writer = Arc::clone(&writer);
        let mut notifications = crate::server::notification_broadcast().subscribe();
        tokio::spawn(async move {
            loop {
                match notifications.recv().await {
                    Ok((channel, payload)) => {
                        if write_frame(&writer, &IpcFrame::Notification { channel, payload })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let mut reader = BufReader::new(tokio::io::stdin());
    let mut authenticated = false;
    let expected_token = std::env::var(STDIO_TOKEN_ENV).unwrap_or_default();
    let max_concurrent = std::env::var("TIANGONG_SIDECAR_MAX_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS);
    let permits = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let active: Arc<tokio::sync::Mutex<HashMap<String, ActiveRequest>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .await
            .context("读取 stdio 帧失败")?;
        if bytes == 0 {
            tracing::info!(service = %service_name, "stdin 已关闭（宿主退出），stdio sidecar 退出");
            terminate_owned_process_group();
            // EOF 是进程级关闭，不只是结束异步主循环。返回到 Tokio runtime
            // 会等待无法取消的 spawn_blocking 任务，文件 IO 卡住时可能永不退出。
            // 进程组不可终止或不归本进程所有时，也必须结束当前 stdio 服务。
            std::process::exit(0);
        }
        let Ok(frame) = serde_json::from_str::<IpcFrame>(line.trim_end()) else {
            tracing::warn!(service = %service_name, "stdio 收到无法解析的帧");
            continue;
        };
        match frame {
            IpcFrame::Auth(auth) => {
                if auth.token != expected_token {
                    let _ = write_frame(
                        &writer,
                        &IpcFrame::Error {
                            message: "stdio 认证失败：token 不匹配".to_string(),
                        },
                    )
                    .await;
                    bail!("stdio sidecar 认证失败");
                }
                authenticated = true;
            }
            IpcFrame::Request(request) => {
                if !authenticated {
                    let _ = write_frame(
                        &writer,
                        &IpcFrame::Error {
                            message: "stdio 首帧必须是 Auth".to_string(),
                        },
                    )
                    .await;
                    bail!("stdio sidecar 在认证前收到请求");
                }
                let context = request
                    .payload
                    .get("context")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok());
                let plugin_request = match serde_json::from_value::<PluginRequest>(request.payload)
                {
                    Ok(plugin_request) => plugin_request,
                    Err(error) => {
                        let response = PluginResponse::error(
                            &request.request_id,
                            PluginErrorCode::BadRequest,
                            format!("解析插件 sidecar 请求失败: {error}"),
                            false,
                        );
                        let _ = respond(&writer, request.request_id, response).await;
                        continue;
                    }
                };
                let request_id = request.request_id;
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                active
                    .lock()
                    .await
                    .insert(request_id.clone(), ActiveRequest { cancel: cancel_tx });
                let permits_for_task = Arc::clone(&permits);
                let writer_for_task = Arc::clone(&writer);
                let service_for_task = Arc::clone(&service_obj);
                let active_for_task = Arc::clone(&active);
                let service_for_log = service_name.clone();
                tokio::spawn(async move {
                    let permit = match permits_for_task.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let progress = ProgressHandle {
                        writer: Arc::clone(&writer_for_task),
                        request_id: request_id.clone(),
                    };
                    // 取消钩子由本任务在 dispatch future 销毁（select 进入
                    // 取消分支）之后、并发许可释放之后调用：既保证钩子看
                    // 到 dispatch 的全部副作用（如终端取消目标登记——与
                    // dispatch 并行会在终端创建与登记之间查询，登记随后的
                    // 完成无人清理，终端残留预留状态），又不在秒级清理期间
                    // 占住并发许可，后续请求可立即处理。
                    let cancel_request = plugin_request.clone();
                    let response = REQUEST_PROGRESS
                        .scope(progress, REQUEST_CONTEXT.scope(context, async {
                            tokio::select! {
                                response = service_for_task.dispatch(plugin_request) => Some(response),
                                _ = cancel_rx => None,
                            }
                        }))
                        .await;
                    let cancelled = response.is_none();
                    active_for_task.lock().await.remove(&request_id);
                    drop(permit);
                    if cancelled {
                        if let Err(error) = service_for_task.cancel(&cancel_request).await {
                            tracing::warn!(
                                service = %service_for_log,
                                request_id = %request_id,
                                %error,
                                "stdio 取消清理失败"
                            );
                        }
                    } else if let Some(response) = response
                        && let Err(error) = respond(&writer_for_task, request_id, response).await
                    {
                        tracing::warn!(service = %service_for_log, %error, "stdio 请求处理失败");
                    }
                });
            }
            IpcFrame::Cancel { request_id } => {
                // 控制帧直接处理，不等待普通请求并发 permit。取消清理
                // （可能是长动作）由执行任务在 dispatch future 销毁后的
                // 取消分支内执行，读循环只发信号、立即回取消响应。
                let target = active.lock().await.remove(&request_id);
                if let Some(target) = target {
                    let _ = target.cancel.send(());
                }
                let response = PluginResponse::error(
                    &request_id,
                    PluginErrorCode::Cancelled,
                    "请求已取消",
                    false,
                );
                if let Err(error) = respond(&writer, request_id, response).await {
                    tracing::warn!(service = %service_name, %error, "stdio 取消响应失败");
                }
            }
            other => {
                tracing::warn!(service = %service_name, frame = ?other, "stdio 收到非预期帧");
            }
        }
    }
}

/// Launcher 启动的 stdio sidecar 是独立进程组组长。宿主异常退出时 stdin
/// 会先关闭；此处终止整个组，避免已启动的后台 Shell 进程成为孤儿。
#[cfg(unix)]
fn terminate_owned_process_group() {
    if std::env::var("TIANGONG_SIDECAR_OWN_PROCESS_GROUP")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let pid = unsafe { libc::getpid() };
    let group = unsafe { libc::getpgrp() };
    if group == pid {
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_owned_process_group() {}

/// macOS 没有 Linux 的 PDEATHSIG。独立线程通过 kqueue 监视创建本次 stdio
/// 连接的实际宿主 PID，因此业务 dispatch 长时间占用主循环时也能立即清理。
#[cfg(target_os = "macos")]
fn start_host_process_monitor() -> Result<()> {
    let host_pid = std::env::var(HOST_PID_ENV)
        .with_context(|| format!("读取 {HOST_PID_ENV} 失败"))?
        .parse::<libc::pid_t>()
        .with_context(|| format!("解析 {HOST_PID_ENV} 失败"))?;
    if host_pid <= 1 || host_pid == unsafe { libc::getpid() } {
        bail!("stdio sidecar 宿主 PID 无效: {host_pid}");
    }
    if !process_alive(host_pid) {
        bail!("stdio sidecar 宿主进程已退出: {host_pid}");
    }

    std::thread::Builder::new()
        .name("sidecar-host-watch".to_string())
        .spawn(move || {
            if !wait_for_process_exit(host_pid) {
                while process_alive(host_pid) {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            terminate_owned_process_group();
            unsafe { libc::_exit(1) };
        })
        .context("启动 macOS sidecar 宿主监视线程失败")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn start_host_process_monitor() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn process_alive(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 返回 true 表示 kqueue 已观察到进程退出；注册或等待不可用时返回 false，
/// 调用方改用有界轮询，避免受限 Seatbelt 环境失去父进程保护。
#[cfg(target_os = "macos")]
fn wait_for_process_exit(pid: libc::pid_t) -> bool {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return false;
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let registered =
        unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if registered < 0 {
        unsafe { libc::close(queue) };
        return false;
    }

    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    loop {
        let received =
            unsafe { libc::kevent(queue, std::ptr::null(), 0, &mut event, 1, std::ptr::null()) };
        if received > 0 {
            unsafe { libc::close(queue) };
            return true;
        }
        if received < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        unsafe { libc::close(queue) };
        return false;
    }
}

struct ActiveRequest {
    cancel: tokio::sync::oneshot::Sender<()>,
}

async fn respond(
    writer: &Arc<tokio::sync::Mutex<tokio::io::Stdout>>,
    request_id: String,
    plugin_response: PluginResponse,
) -> Result<()> {
    let payload =
        serde_json::to_value(&plugin_response).with_context(|| "序列化 sidecar 响应失败")?;
    write_frame(
        writer,
        &IpcFrame::Response(IpcResponse {
            request_id,
            payload,
        }),
    )
    .await
}

async fn write_frame(
    writer: &Arc<tokio::sync::Mutex<tokio::io::Stdout>>,
    frame: &IpcFrame,
) -> Result<()> {
    let line = serde_json::to_string(frame).with_context(|| "序列化 stdio 帧失败")?;
    let mut stdout = writer.lock().await;
    stdout
        .write_all(line.as_bytes())
        .await
        .with_context(|| "写入 stdio 帧失败")?;
    stdout
        .write_all(b"\n")
        .await
        .with_context(|| "写入 stdio 换行失败")?;
    stdout.flush().await.with_context(|| "刷新 stdio 帧失败")?;
    Ok(())
}
