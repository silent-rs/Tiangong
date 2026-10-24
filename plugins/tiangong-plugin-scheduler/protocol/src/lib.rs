//! Scheduler 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、文件系统或 Wasmtime 依赖。可同时编译为本机与 `wasm32-wasip2`。
//!
//! 与定时任务核心库 [`tiangong_scheduler`] 解耦：协议在这里用自带的可序列化结构，
//! sidecar 在内部把它们映射到 `tiangong_scheduler::model`/`store`，避免把核心库
//! 拖进 WASM 编译。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PLUGIN_ID: &str = "scheduler";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SCHEDULER_PROTOCOL_VERSION: u32 = 1;
/// @提及候选查询操作：与清单 `mention.candidates_op` 一致（含插件前缀）。
pub const MENTION_CANDIDATES_OPERATION: &str = "scheduler.mention_candidates";

/// 工具名常量（与工具规格、handle_tool 路由对齐）。
pub const TOOL_CREATE_JOB: &str = "scheduler_create_job";
pub const TOOL_LIST_JOBS: &str = "scheduler_list_jobs";
pub const TOOL_UPDATE_JOB: &str = "scheduler_update_job";
pub const TOOL_DELETE_JOB: &str = "scheduler_delete_job";
pub const TOOL_TRIGGER_JOB: &str = "scheduler_trigger_job";
pub const TOOL_GET_JOB_RUNS: &str = "scheduler_get_job_runs";

/// 一个类型化 Scheduler 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// WASM 端通过 `sidecar_client::invoke::<O>()` 泛型调用，以 `NAME` 作为 operation、
/// 序列化 `Request`、反序列化 `Response`。
pub trait SchedulerOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

// ── 操作名常量 ────────────────────────────────────────────────

pub const CREATE_JOB_OPERATION: &str = "create_job";
pub const LIST_JOBS_OPERATION: &str = "list_jobs";
pub const UPDATE_JOB_OPERATION: &str = "update_job";
pub const DELETE_JOB_OPERATION: &str = "delete_job";
pub const TRIGGER_JOB_OPERATION: &str = "trigger_job";
pub const GET_JOB_RUNS_OPERATION: &str = "get_job_runs";

// ── marker 类型 ───────────────────────────────────────────────

pub struct CreateJob;
pub struct ListJobs;
pub struct UpdateJob;
pub struct DeleteJob;
pub struct TriggerJob;
pub struct GetJobRuns;

impl SchedulerOperation for CreateJob {
    const NAME: &'static str = CREATE_JOB_OPERATION;
    type Request = CreateJobRequest;
    type Response = JobResponse;
}

impl SchedulerOperation for ListJobs {
    const NAME: &'static str = LIST_JOBS_OPERATION;
    type Request = Empty;
    type Response = ListJobsResponse;
}

impl SchedulerOperation for UpdateJob {
    const NAME: &'static str = UPDATE_JOB_OPERATION;
    type Request = UpdateJobRequest;
    type Response = JobResponse;
}

impl SchedulerOperation for DeleteJob {
    const NAME: &'static str = DELETE_JOB_OPERATION;
    type Request = DeleteJobRequest;
    type Response = DeleteJobResponse;
}

impl SchedulerOperation for TriggerJob {
    const NAME: &'static str = TRIGGER_JOB_OPERATION;
    type Request = TriggerJobRequest;
    type Response = TriggerJobResponse;
}

impl SchedulerOperation for GetJobRuns {
    const NAME: &'static str = GET_JOB_RUNS_OPERATION;
    type Request = GetJobRunsRequest;
    type Response = GetJobRunsResponse;
}

// ── 通用结构 ──────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ack {
    #[serde(default)]
    pub ok: bool,
}

/// 触发类型（与 `tiangong_scheduler::model::TriggerType` 对齐）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    #[default]
    Cron,
}

/// Job 运行状态（与 `tiangong_scheduler::model::JobRunStatus` 对齐）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobRunStatus {
    Running,
    #[default]
    Succeeded,
    Failed,
}

/// 定时任务定义（协议层自带，sidecar 内部与 `tiangong_scheduler::model::Job` 互转）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub description: String,
    pub trigger_type: TriggerType,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub payload: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Job 单次执行记录（与 `tiangong_scheduler::model::JobRun` 对齐）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    pub session_id: String,
    pub status: JobRunStatus,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub result_summary: Option<String>,
}

// ── 请求/响应类型 ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateJobRequest {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub payload: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// 更新 Job 请求。所有字段可选，`None` 表示不更新。
///
/// 与 `tiangong_scheduler::model::UpdateJobRequest` 对齐：`schedule`/`session_id`
/// 为 `Option<String>`（`Some("")` 表示清空原值，`None` 表示保持不变）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateJobRequest {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteJobRequest {
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerJobRequest {
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetJobRunsRequest {
    pub id: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobResponse {
    pub job: Job,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListJobsResponse {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteJobResponse {
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerJobResponse {
    pub triggered: bool,
    pub job: Job,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetJobRunsResponse {
    pub runs: Vec<JobRun>,
}

const fn default_true() -> bool {
    true
}

const fn default_limit() -> usize {
    10
}
