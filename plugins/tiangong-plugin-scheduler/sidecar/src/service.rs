//! Scheduler sidecar 业务服务：承载 cron 调度、JobStore 与到点 HTTP 投递。
//!
//! 整合原进程内插件 `handler.rs` 的工具执行（CRUD + trigger）与 `executor.rs` 的
//! 调度执行逻辑，全部经 IPC 操作暴露给运行时（host 侧 invoke_sidecar）与 WASM 桥接。
//!
//! 到点触发时经 HTTP 调本机 server 的 `POST /api/v1/messages` 投递消息，与 Bot/webhook
//! 同链路，不再依赖 host 注入的 `SchedulerContext` 回调。

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};
use tiangong_plugin_runtime::sidecar::{PLUGIN_DATA_DIR_ENV, STORAGE_ROOT_ENV};
use tiangong_plugin_scheduler_protocol::{
    CreateJobRequest, DeleteJobRequest, Empty, GetJobRunsRequest, Job, JobResponse, JobRun,
    JobRunStatus, ListJobsResponse, PLUGIN_ID, PLUGIN_VERSION, SCHEDULER_PROTOCOL_VERSION,
    TriggerJobRequest, TriggerJobResponse, TriggerType, UpdateJobRequest,
};
use tiangong_scheduler::store::JobStore;

/// Scheduler sidecar 业务服务。
pub struct SchedulerService {
    /// 任务存储根（默认 `~/.tiangong/scheduler`，由 env 覆盖）。
    store: JobStore,
    /// HTTP 投递客户端。
    deliver: Arc<MessageDeliver>,
}

/// 到点消息投递器：经 HTTP 调本机 server 的 `POST /api/v1/messages`。
struct MessageDeliver {
    client: reqwest::Client,
    server_url: Option<String>,
    server_token: Option<String>,
    /// 捕获的 tokio runtime 句柄：silent 调度器用 async-global-executor 执行回调，
    /// 回调里 await tokio 异步代码会 panic，这里把句柄捕获进闭包，回调被触发时
    /// 用 handle.spawn 把真正执行投递回 tokio runtime（与原 restore_cron_jobs 同模式）。
    handle: tokio::runtime::Handle,
}

impl MessageDeliver {
    fn new(handle: tokio::runtime::Handle) -> Self {
        let server_url = std::env::var("TIANGONG_SERVER_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let server_token = std::env::var("TIANGONG_SERVER_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        // server 收到 scheduler 消息后会立即返回 202（respond-async），不需要等整轮
        // turn 完成，因此 30s 超时足够覆盖 server 启动/繁忙场景。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            server_url,
            server_token,
            handle,
        }
    }

    /// 投递消息到本机 server。
    ///
    /// 用 `connector="server-api"` + `channel_id=session_id`，让 server 把 channel_id
    /// 直接当 session_id 用（与 `/api/v1/chat` 同路径，零改动 server 路由）。
    /// `render` 为插件渲染声明：界面据此以本插件的定时任务卡片显示，正文
    /// `message` 保持完整供模型读取与旧版宿主兜底。
    ///
    /// 请求带 `Prefer: respond-async` 头，server 收到后立即返回 202，整轮 turn 在后台
    /// 异步执行。因此本调用只需确认「消息已被 server 接收」，不等整轮完成。
    /// 仍然检查响应状态码：401/403/500 等服务端错误会被识别为投递失败。
    async fn deliver(
        &self,
        session_id: &str,
        content: String,
        render: serde_json::Value,
    ) -> Result<()> {
        let url = self
            .server_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("TIANGONG_SERVER_URL 未配置，定时任务无法投递消息"))?;
        let endpoint = format!("{}/api/v1/messages", url.trim_end_matches('/'));
        let body = serde_json::json!({
            "connector": "server-api",
            "channel_id": session_id,
            "message": content,
            "render": render,
        });
        let mut request = self
            .client
            .post(&endpoint)
            .json(&body)
            .header("Prefer", "respond-async");
        if let Some(token) = self.server_token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("投递定时任务消息失败: {endpoint}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "server 拒绝定时任务消息: HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            );
        }
        Ok(())
    }
}

impl SchedulerService {
    /// 构造服务：打开 JobStore、初始化 HTTP 投递器、恢复已启用的 cron 任务到调度器。
    pub fn new() -> Result<Self> {
        let store = open_store()?;
        let handle = tokio::runtime::Handle::current();
        let deliver = Arc::new(MessageDeliver::new(handle));
        let service = Self { store, deliver };
        service.restore_cron_jobs();
        Ok(service)
    }

    /// 按 sidecar 协议分发请求。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Scheduler 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let payload = match self
            .dispatch_operation(&request.operation, request.payload)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    error.to_string(),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }

    async fn dispatch_operation(
        &self,
        operation: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match operation {
            HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: SCHEDULER_PROTOCOL_VERSION,
                capabilities: vec!["scheduler".to_string()],
                instance_id: format!("scheduler-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Scheduler 握手响应失败"),
            "create_job" => {
                let req: CreateJobRequest =
                    serde_json::from_value(payload).with_context(|| "解析 create_job 请求失败")?;
                // 校验 cron 表达式（需 6 字段）
                if let Some(schedule) = req.schedule.as_deref() {
                    validate_cron_schedule(schedule)?;
                }
                let job = self.create_job(req)?;
                serde_json::to_value(JobResponse { job })
                    .with_context(|| "序列化 create_job 响应失败")
            }
            "list_jobs" => {
                let _payload: Empty = serde_json::from_value(payload).unwrap_or_default();
                let jobs = self
                    .store
                    .list_jobs()
                    .context("查询任务列表失败")?
                    .into_iter()
                    .map(job_to_protocol)
                    .collect::<Vec<_>>();
                serde_json::to_value(ListJobsResponse { jobs })
                    .with_context(|| "序列化 list_jobs 响应失败")
            }
            "update_job" => {
                let req: UpdateJobRequest =
                    serde_json::from_value(payload).with_context(|| "解析 update_job 请求失败")?;
                // 更新 schedule 时同样校验（非空才校验）
                if let Some(schedule) = req.schedule.as_deref().filter(|s| !s.is_empty()) {
                    validate_cron_schedule(schedule)?;
                }
                let job = self.update_job(req)?;
                serde_json::to_value(JobResponse { job })
                    .with_context(|| "序列化 update_job 响应失败")
            }
            "delete_job" => {
                let req: DeleteJobRequest =
                    serde_json::from_value(payload).with_context(|| "解析 delete_job 请求失败")?;
                let deleted = self.delete_job(&req.id)?;
                serde_json::to_value(tiangong_plugin_scheduler_protocol::DeleteJobResponse {
                    deleted,
                })
                .with_context(|| "序列化 delete_job 响应失败")
            }
            "trigger_job" => {
                let req: TriggerJobRequest =
                    serde_json::from_value(payload).with_context(|| "解析 trigger_job 请求失败")?;
                let job = self
                    .store
                    .get_job(&req.id)
                    .with_context(|| "查询任务失败")?
                    .ok_or_else(|| anyhow::anyhow!("定时任务 '{}' 不存在", req.id))?;
                let response_job = job_to_protocol(job.clone());
                // 异步派发执行（不阻塞当前请求）
                self.spawn_execute(job);
                serde_json::to_value(TriggerJobResponse {
                    triggered: true,
                    job: response_job,
                })
                .with_context(|| "序列化 trigger_job 响应失败")
            }
            "get_job_runs" => {
                let req: GetJobRunsRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 get_job_runs 请求失败")?;
                // 先校验任务存在
                let exists = self
                    .store
                    .get_job(&req.id)
                    .with_context(|| "查询任务失败")?
                    .is_some();
                if !exists {
                    anyhow::bail!("定时任务 '{}' 不存在", req.id);
                }
                let limit = if req.limit == 0 { 10 } else { req.limit };
                let runs = self
                    .store
                    .list_job_runs(&req.id, limit)
                    .context("查询执行历史失败")?
                    .into_iter()
                    .map(job_run_to_protocol)
                    .collect::<Vec<_>>();
                serde_json::to_value(tiangong_plugin_scheduler_protocol::GetJobRunsResponse {
                    runs,
                })
                .with_context(|| "序列化 get_job_runs 响应失败")
            }
            other => Err(anyhow::anyhow!("未知的 Scheduler 操作: {other}")),
        }
    }

    // ── 业务实现 ──────────────────────────────────────────────────

    fn create_job(&self, req: CreateJobRequest) -> Result<Job> {
        let now = chrono::Local::now().naive_local().to_string();
        let model = tiangong_scheduler::model::Job {
            id: scru128::new().to_string(),
            name: req.name,
            description: req.description,
            trigger_type: tiangong_scheduler::model::TriggerType::Cron,
            schedule: req.schedule,
            session_id: req.session_id,
            payload: req.payload,
            enabled: req.enabled,
            created_at: now.clone(),
            updated_at: now,
        };
        let inserted = self.store.insert_job(&model).context("写入任务失败")?;
        let job = job_to_protocol(inserted);
        // 启用的 cron 任务需同步注册到调度器
        self.sync_schedule_for_job(&job);
        Ok(job)
    }

    fn update_job(&self, req: UpdateJobRequest) -> Result<Job> {
        let update = tiangong_scheduler::model::UpdateJobRequest {
            name: req.name,
            description: req.description,
            schedule: req.schedule,
            session_id: req.session_id,
            payload: req.payload,
            enabled: req.enabled,
        };
        let updated = self
            .store
            .update_job(&req.id, &update)
            .context("更新任务失败")?;
        if !updated {
            anyhow::bail!("定时任务 '{}' 不存在", req.id);
        }
        let model = self
            .store
            .get_job(&req.id)
            .context("查询更新后的任务失败")?
            .ok_or_else(|| anyhow::anyhow!("定时任务 '{}' 不存在", req.id))?;
        let job = job_to_protocol(model);
        self.sync_schedule_for_job(&job);
        Ok(job)
    }

    fn delete_job(&self, id: &str) -> Result<bool> {
        let deleted = self.store.delete_job(id).context("删除任务失败")?;
        if deleted {
            // 从调度器移除
            self.remove_scheduled(id);
        }
        Ok(deleted)
    }

    /// 异步派发任务执行（fire-and-forget）。
    fn spawn_execute(&self, job: tiangong_scheduler::model::Job) {
        let store = self.store.clone();
        let deliver = self.deliver.clone();
        tokio::spawn(async move {
            execute_core(&store, &deliver, job).await;
        });
    }

    // ── cron 调度同步 ─────────────────────────────────────────────

    /// 从 JobStore 加载已启用的 cron job 注册到 silent SCHEDULER，并启动调度循环。
    fn restore_cron_jobs(&self) {
        let jobs = match self.store.list_enabled_cron_jobs() {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(%error, "恢复定时任务失败，跳过");
                return;
            }
        };

        let handle = self.deliver.handle.clone();
        let store = self.store.clone();
        let deliver = self.deliver.clone();

        tokio::task::block_in_place(|| {
            handle.block_on(async {
                // 先清空可能残留的旧任务（避免重复注册）
                {
                    let mut scheduler = silent::SCHEDULER.lock().await;
                    let ids: Vec<String> =
                        scheduler.get_tasks().iter().map(|t| t.id.clone()).collect();
                    for id in ids {
                        scheduler.remove_task(&id);
                    }
                }
                for job in jobs {
                    register_cron_task(&job, &store, &deliver, &handle).await;
                }
            });
        });

        // 启动调度循环（幂等：重复 spawn 多个循环不影响正确性，但这里只起一次）
        static SCHEDULER_STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if SCHEDULER_STARTED.set(()).is_ok() {
            tokio::spawn(async {
                silent::Scheduler::schedule(silent::SCHEDULER.clone()).await;
            });
            tracing::info!("scheduler sidecar 已启动 cron 调度循环");
        }
    }

    /// create/update 后同步调度器中的单个任务。
    fn sync_schedule_for_job(&self, job: &Job) {
        // 先移除旧任务（若存在），再按最新状态注册
        self.remove_scheduled(&job.id);
        if !job.enabled {
            return;
        }
        let Some(schedule) = job.schedule.as_deref() else {
            return;
        };
        let store = self.store.clone();
        let deliver = self.deliver.clone();
        let handle = self.deliver.handle.clone();
        // 把 model 转回去
        let model = job_to_model(job.clone());
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                register_cron_task_with_schedule(&model, schedule, &store, &deliver, &handle).await;
            });
        });
    }

    fn remove_scheduled(&self, id: &str) {
        let id = id.to_string();
        let handle = self.deliver.handle.clone();
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                silent::SCHEDULER.lock().await.remove_task(&id);
            });
        });
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for SchedulerService {
    async fn dispatch(&self, request: Request) -> Response {
        SchedulerService::dispatch(self, request).await
    }
}

// ── 执行核心（替代原 SchedulerContext::send_message）─────────────────

/// 执行定时任务：防重叠 → 记录 Running → HTTP 投递 → 记录 Succeeded/Failed。
async fn execute_core(
    store: &JobStore,
    deliver: &Arc<MessageDeliver>,
    job: tiangong_scheduler::model::Job,
) {
    // 重新读取最新任务状态（可能已被 update/disable）
    let fresh = store.get_job(&job.id).ok().flatten().unwrap_or(job);

    // 防止同一任务重叠执行
    {
        let mut running = RUNNING.lock().unwrap();
        if running.contains(&fresh.id) {
            tracing::warn!(job_id = %fresh.id, "触发跳过：上一轮执行尚未完成");
            return;
        }
        running.insert(fresh.id.clone());
    }
    struct RunGuard(String);
    impl Drop for RunGuard {
        fn drop(&mut self) {
            RUNNING.lock().unwrap().remove(&self.0);
        }
    }
    let _guard = RunGuard(fresh.id.clone());

    let run_id = scru128::new().to_string();
    let now = chrono::Local::now().naive_local().to_string();

    // 解析 session_id：有则复用，无则分配新 id（投递成功后写回）
    let (session_id, created_new) = resolve_session_id(store, fresh.session_id.as_deref());

    // 记录开始执行
    let run = tiangong_scheduler::model::JobRun {
        id: run_id.clone(),
        job_id: fresh.id.clone(),
        session_id: session_id.clone(),
        status: tiangong_scheduler::model::JobRunStatus::Running,
        started_at: now,
        finished_at: None,
        result_summary: None,
    };
    if let Err(error) = store.insert_job_run(&run) {
        tracing::error!(job_id = %fresh.id, %error, "记录 JobRun 失败");
    }

    // 构造消息（与原实现一致的头部，兼容历史消息前端解析）
    let message = format!(
        "[定时任务触发]\n任务名称：{}\n任务描述：{}\n\n{}",
        fresh.name, fresh.description, fresh.payload
    );
    let render = scheduled_task_render(&fresh, &run_id);

    let finished_at = chrono::Local::now().naive_local().to_string();
    match deliver.deliver(&session_id, message, render).await {
        Ok(()) => {
            // 投递成功后才把新 session_id 写回 job
            if created_new {
                let req = tiangong_scheduler::model::UpdateJobRequest {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                };
                if let Err(error) = store.update_job(&fresh.id, &req) {
                    tracing::warn!(job_id = %fresh.id, %error, "写回 session_id 失败");
                } else {
                    tracing::info!(job_id = %fresh.id, session_id = %session_id, "已绑定会话");
                }
            }
            let _ = store.update_job_run_status(
                &run_id,
                &fresh.id,
                &tiangong_scheduler::model::JobRunStatus::Succeeded,
                Some(&finished_at),
                Some("消息已发送至会话"),
            );
            tracing::info!(job_id = %fresh.id, "触发消息已发送");
        }
        Err(error) => {
            let err_msg = format!("发送失败：{error}");
            let _ = store.update_job_run_status(
                &run_id,
                &fresh.id,
                &tiangong_scheduler::model::JobRunStatus::Failed,
                Some(&finished_at),
                Some(&err_msg),
            );
            tracing::error!(job_id = %fresh.id, %error, "触发发送失败");
        }
    }
}

/// 定时任务卡片的 `session.message-item` 贡献 ID（见 plugin.json）。
const SCHEDULED_TASK_VIEW: &str = "scheduled-task";

/// 触发消息的渲染声明（`POST /api/v1/messages` 的 `render` 字段）。
///
/// `data` 只带卡片头部所需的小字段；任务名称、描述与执行内容由视图从消息
/// 正文读取（正文是唯一来源，搜索高亮也基于正文），避免大段文本重复携带
/// （宿主限制 render 序列化后不超过 16KB，超限会拒绝整条消息）。
fn scheduled_task_render(job: &tiangong_scheduler::model::Job, run_id: &str) -> serde_json::Value {
    serde_json::json!({
        "plugin": PLUGIN_ID,
        "view": SCHEDULED_TASK_VIEW,
        "data": {
            "job_id": job.id,
            "run_id": run_id,
            "schedule": job.schedule,
        },
    })
}

/// 解析 session_id：有 session_id 且存储中存在则复用；否则生成新 id。
///
/// 与原 `ServerSchedulerContext::resolve_session_id` 同源：仅按磁盘落盘的会话判定
/// 存在性。sidecar 无法直接查询 server 的 session 列表，因此首次触发（无 session_id）
/// 生成新 scru128，投递成功后写回 job。下一次触发复用该 id（server 会用该 id
/// 创建/复用 Core）。
fn resolve_session_id(_store: &JobStore, requested_session_id: Option<&str>) -> (String, bool) {
    if let Some(sid) = requested_session_id.filter(|s| !s.is_empty()) {
        return (sid.to_string(), false);
    }
    (scru128::new().to_string(), true)
}

/// 正在执行的 job_id 集合，防止同一任务重叠执行。
static RUNNING: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

// ── cron 调度注册 ─────────────────────────────────────────────────

/// 注册一个 cron 任务到 silent SCHEDULER（用 job 最新数据读取 schedule）。
async fn register_cron_task(
    job: &tiangong_scheduler::model::Job,
    store: &JobStore,
    deliver: &Arc<MessageDeliver>,
    handle: &tokio::runtime::Handle,
) {
    let Some(schedule) = job.schedule.as_deref() else {
        return;
    };
    register_cron_task_with_schedule(job, schedule, store, deliver, handle).await;
}

/// 注册一个 cron 任务到 silent SCHEDULER（schedule 已解析）。
async fn register_cron_task_with_schedule(
    job: &tiangong_scheduler::model::Job,
    schedule: &str,
    store: &JobStore,
    deliver: &Arc<MessageDeliver>,
    handle: &tokio::runtime::Handle,
) {
    let process_time = match schedule.try_into() {
        Ok(pt) => pt,
        Err(error) => {
            tracing::warn!(job_id = %job.id, schedule, %error, "解析 cron 表达式失败，跳过注册");
            return;
        }
    };
    let task = build_cron_task(job, process_time, store.clone(), deliver.clone(), handle);
    let mut scheduler = silent::SCHEDULER.lock().await;
    match scheduler.add_task(task) {
        Ok(()) => tracing::info!(job_id = %job.id, name = %job.name, "已注册定时任务"),
        Err(error) => tracing::warn!(job_id = %job.id, %error, "注册定时任务失败"),
    }
}

/// 构造 silent cron Task：到点把 execute_core 投递到 tokio runtime。
///
/// silent 的调度器用 async-global-executor 执行回调（非 tokio 线程池），直接 await
/// tokio 异步逻辑会因找不到 reactor 而 panic。这里捕获 tokio runtime 句柄，回调触发
/// 时用 `Handle::spawn` 把执行转发回 tokio runtime，回调立即返回。
fn build_cron_task(
    job: &tiangong_scheduler::model::Job,
    process_time: silent::ProcessTime,
    store: JobStore,
    deliver: Arc<MessageDeliver>,
    handle: &tokio::runtime::Handle,
) -> silent::Task {
    let job = job.clone();
    let handle = handle.clone();
    silent::Task::create_with_action_async(
        job.id.clone(),
        process_time,
        job.name.clone(),
        Arc::new(move || {
            let job = job.clone();
            let store = store.clone();
            let deliver = deliver.clone();
            let handle = handle.clone();
            Box::pin(async move {
                tracing::info!(job_id = %job.id, name = %job.name, "定时任务触发");
                handle.spawn(async move { execute_core(&store, &deliver, job).await });
                Ok(())
            })
        }),
    )
}

// ── 辅助函数 ─────────────────────────────────────────────────────

/// 打开 JobStore：优先使用宿主共享存储根，确保两个设置入口共用原有任务数据。
fn open_store() -> Result<JobStore> {
    if let Ok(storage_root) = std::env::var(STORAGE_ROOT_ENV)
        && !storage_root.is_empty()
    {
        let base = std::path::PathBuf::from(storage_root).join("scheduler");
        return JobStore::open_at(base);
    }
    // 独立运行且宿主未注入共享根目录时，才回退到插件私有数据目录。
    if let Ok(data_dir) = std::env::var(PLUGIN_DATA_DIR_ENV)
        && !data_dir.is_empty()
    {
        let base = std::path::PathBuf::from(data_dir).join("scheduler");
        return JobStore::open_at(base);
    }
    JobStore::open()
}

/// 校验 schedule 字符串能否被调度器解析（需 6~7 字段 cron 表达式）。
fn validate_cron_schedule(expr: &str) -> Result<()> {
    tiangong_scheduler::executor::validate_cron_schedule(expr).with_context(|| {
        format!("schedule 不是合法的 cron 表达式（需 6 字段，如 '0 25 21 * * *'）：{expr}")
    })
}

// ── 协议层与核心库模型互转 ────────────────────────────────────────
//
// protocol crate（可编译到 WASM）不依赖 tiangong_scheduler（含 silent/cron 原生依赖），
// 因此转换在 sidecar 这一层用普通函数完成（孤儿规则不允许跨 crate impl From）。

fn job_to_protocol(model: tiangong_scheduler::model::Job) -> Job {
    Job {
        id: model.id,
        name: model.name,
        description: model.description,
        trigger_type: match model.trigger_type {
            tiangong_scheduler::model::TriggerType::Cron => TriggerType::Cron,
        },
        schedule: model.schedule,
        session_id: model.session_id,
        payload: model.payload,
        enabled: model.enabled,
        created_at: model.created_at,
        updated_at: model.updated_at,
    }
}

fn job_to_model(job: Job) -> tiangong_scheduler::model::Job {
    let trigger_type = match job.trigger_type {
        TriggerType::Cron => tiangong_scheduler::model::TriggerType::Cron,
    };
    tiangong_scheduler::model::Job {
        id: job.id,
        name: job.name,
        description: job.description,
        trigger_type,
        schedule: job.schedule,
        session_id: job.session_id,
        payload: job.payload,
        enabled: job.enabled,
        created_at: job.created_at,
        updated_at: job.updated_at,
    }
}

fn job_run_to_protocol(model: tiangong_scheduler::model::JobRun) -> JobRun {
    let status = match model.status {
        tiangong_scheduler::model::JobRunStatus::Running => JobRunStatus::Running,
        tiangong_scheduler::model::JobRunStatus::Succeeded => JobRunStatus::Succeeded,
        tiangong_scheduler::model::JobRunStatus::Failed => JobRunStatus::Failed,
    };
    JobRun {
        id: model.id,
        job_id: model.job_id,
        session_id: model.session_id,
        status,
        started_at: model.started_at,
        finished_at: model.finished_at,
        result_summary: model.result_summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 触发消息渲染声明指向本插件视图且不重复携带正文() {
        let job = tiangong_scheduler::model::Job {
            id: "job-1".into(),
            name: "日报".into(),
            description: "每天汇总".into(),
            trigger_type: tiangong_scheduler::model::TriggerType::Cron,
            schedule: Some("0 0 9 * * *".into()),
            session_id: None,
            payload: "很长的执行内容".repeat(4096),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let render = scheduled_task_render(&job, "run-1");
        assert_eq!(render["plugin"], PLUGIN_ID);
        assert_eq!(render["view"], SCHEDULED_TASK_VIEW);
        assert_eq!(render["data"]["job_id"], "job-1");
        assert_eq!(render["data"]["run_id"], "run-1");
        assert_eq!(render["data"]["schedule"], "0 0 9 * * *");
        // 执行内容只在正文里，渲染声明保持小体积。
        assert!(serde_json::to_vec(&render).unwrap().len() < 1024);
    }
}
