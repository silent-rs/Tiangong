use serde::{Deserialize, Serialize};

/// 触发类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    Cron,
}

/// Job 运行状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobRunStatus {
    Running,
    Succeeded,
    Failed,
}

/// 定时任务定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub description: String,
    pub trigger_type: TriggerType,
    /// Cron 表达式
    #[serde(default)]
    pub schedule: Option<String>,
    /// 可选，关联已有 session 复用上下文；为空时触发自动创建新 session
    #[serde(default)]
    pub session_id: Option<String>,
    /// 触发时构造给 LLM 的任务描述
    pub payload: String,
    /// 是否启用
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 创建 Job 请求
#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub name: String,
    pub description: String,
    pub trigger_type: TriggerType,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub payload: String,
    #[serde(default = "crate::default_true")]
    pub enabled: bool,
}

/// 更新 Job 请求（所有字段可选）
#[derive(Debug, Default, Deserialize)]
pub struct UpdateJobRequest {
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

/// Job 单次执行记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    /// 本次执行使用的 session_id
    pub session_id: String,
    pub status: JobRunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub result_summary: Option<String>,
}
