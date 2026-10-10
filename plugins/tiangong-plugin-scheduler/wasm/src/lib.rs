//! Scheduler 插件的 WASM 桥接组件。
//!
//! 本组件只做桥接：工具规格/工具执行全部转发到 Scheduler sidecar。
//! 重型原生依赖（cron 调度、JobStore、HTTP 投递）全部在 sidecar 进程内运行，
//! WASM 沙箱仅负责参数解析与 IPC 转发。

mod bindings;
mod sidecar_client;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use serde_json::Value;
use tiangong_plugin_scheduler_protocol::{
    CreateJob, CreateJobRequest, DeleteJob, DeleteJobRequest, Empty, GetJobRuns, GetJobRunsRequest,
    Job, JobRun, ListJobs, SchedulerOperation, TOOL_CREATE_JOB, TOOL_DELETE_JOB, TOOL_GET_JOB_RUNS,
    TOOL_LIST_JOBS, TOOL_TRIGGER_JOB, TOOL_UPDATE_JOB, TriggerJob, TriggerJobRequest, UpdateJob,
    UpdateJobRequest,
};

mod descriptor {
    pub const ID: &str = tiangong_plugin_scheduler_protocol::PLUGIN_ID;
    pub const NAME: &str = "Scheduler";
    pub const VERSION: &str = tiangong_plugin_scheduler_protocol::PLUGIN_VERSION;
}

fn plugin_err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

struct Component;

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: descriptor::ID.to_string(),
            name: descriptor::NAME.to_string(),
            version: descriptor::VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(vec![
            ToolSpec {
                name: TOOL_CREATE_JOB.to_string(),
                description: "创建定时任务（Cron Job）。指定 Cron 表达式、任务内容后，调度器会按周期自动触发。"
                    .to_string(),
                input_schema: schema_create_job(),
            },
            ToolSpec {
                name: TOOL_LIST_JOBS.to_string(),
                description: "列出所有定时任务。".to_string(),
                input_schema: r#"{"type":"object","properties":{},"required":[]}"#.to_string(),
            },
            ToolSpec {
                name: TOOL_UPDATE_JOB.to_string(),
                description: "更新已有定时任务的字段（所有字段可选，仅更新传入的字段）。".to_string(),
                input_schema: schema_update_job(),
            },
            ToolSpec {
                name: TOOL_DELETE_JOB.to_string(),
                description: "按 ID 删除定时任务。".to_string(),
                input_schema: r#"{"type":"object","properties":{"id":{"type":"string","description":"任务 ID"}},"required":["id"]}"#
                    .to_string(),
            },
            ToolSpec {
                name: TOOL_TRIGGER_JOB.to_string(),
                description:
                    "按 ID 立即手动触发一次定时任务执行（会真正执行任务并把结果写入执行历史）。"
                        .to_string(),
                input_schema: r#"{"type":"object","properties":{"id":{"type":"string","description":"任务 ID"}},"required":["id"]}"#
                    .to_string(),
            },
            ToolSpec {
                name: TOOL_GET_JOB_RUNS.to_string(),
                description: "查询指定定时任务的最近执行历史。".to_string(),
                input_schema: r#"{"type":"object","properties":{"id":{"type":"string","description":"任务 ID"},"limit":{"type":"integer","description":"返回记录数量，默认 10"}},"required":["id"]}"#
                    .to_string(),
            },
        ])
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        Ok(Vec::new())
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        match call.name.as_str() {
            TOOL_CREATE_JOB => handle_create_job(&call),
            TOOL_LIST_JOBS => handle_list_jobs(&call),
            TOOL_UPDATE_JOB => handle_update_job(&call),
            TOOL_DELETE_JOB => handle_delete_job(&call),
            TOOL_TRIGGER_JOB => handle_trigger_job(&call),
            TOOL_GET_JOB_RUNS => handle_get_job_runs(&call),
            other => Err(plugin_err(format!("未知的 Scheduler 工具: {other}"))),
        }
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }

    fn set_workspace(_workspace: Option<String>, _full_trust: bool) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_config_updated(_config_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ready(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_started(_session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_finished(_session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ended(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }
}

// ── 工具实现 ────────────────────────────────────────────────────

fn handle_create_job(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let name = get_str_field(&args, "name").ok_or_else(|| plugin_err("缺少必填参数 name"))?;
    let description = get_str_field(&args, "description")
        .ok_or_else(|| plugin_err("缺少必填参数 description"))?;
    let schedule =
        get_str_field(&args, "schedule").ok_or_else(|| plugin_err("缺少必填参数 schedule"))?;
    let payload =
        get_str_field(&args, "payload").ok_or_else(|| plugin_err("缺少必填参数 payload"))?;
    let session_id = get_opt_str_field(&args, "session_id");
    let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);

    let request = CreateJobRequest {
        name,
        description,
        schedule: Some(schedule),
        session_id,
        payload,
        enabled,
    };
    let response = sidecar_client::invoke::<CreateJob>(&request)
        .map_err(|e| plugin_err(format!("create_job 执行失败: {e}")))?;
    let output = serde_json::to_string_pretty(&job_summary(&response.job)).unwrap_or_default();
    Ok(ToolResult {
        ok: true,
        summary: format!("已创建定时任务：{}", response.job.name),
        stdout: output,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

fn handle_list_jobs(_call: &ToolCall) -> Result<ToolResult, PluginError> {
    let response =
        sidecar_client::invoke::<ListJobs>(&tiangong_plugin_scheduler_protocol::Empty {})
            .map_err(|e| plugin_err(format!("list_jobs 执行失败: {e}")))?;
    let summary: Vec<Value> = response
        .jobs
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "name": j.name,
                "schedule": j.schedule,
                "enabled": j.enabled,
                "session_id": j.session_id,
            })
        })
        .collect();
    let count = summary.len();
    Ok(ToolResult {
        ok: true,
        summary: format!("共 {count} 个定时任务"),
        stdout: serde_json::to_string_pretty(&summary).unwrap_or_default(),
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

fn handle_update_job(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let id = get_str_field(&args, "id").ok_or_else(|| plugin_err("缺少必填参数 id"))?;
    let request = UpdateJobRequest {
        id,
        name: get_opt_str_field(&args, "name"),
        description: get_opt_str_field(&args, "description"),
        schedule: get_nullable_str_field(&args, "schedule").map(|opt| opt.unwrap_or_default()),
        session_id: get_nullable_str_field(&args, "session_id").map(|opt| opt.unwrap_or_default()),
        payload: get_opt_str_field(&args, "payload"),
        enabled: args.get("enabled").and_then(Value::as_bool),
    };
    let response = sidecar_client::invoke::<UpdateJob>(&request)
        .map_err(|e| plugin_err(format!("update_job 执行失败: {e}")))?;
    let output = serde_json::to_string_pretty(&job_summary(&response.job)).unwrap_or_default();
    Ok(ToolResult {
        ok: true,
        summary: format!("已更新定时任务：{}", response.job.name),
        stdout: output,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

fn handle_delete_job(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let id = get_str_field(&args, "id").ok_or_else(|| plugin_err("缺少必填参数 id"))?;
    let request = DeleteJobRequest { id: id.clone() };
    let response = sidecar_client::invoke::<DeleteJob>(&request)
        .map_err(|e| plugin_err(format!("delete_job 执行失败: {e}")))?;
    Ok(ToolResult {
        ok: response.deleted,
        summary: if response.deleted {
            format!("已删除定时任务：{id}")
        } else {
            format!("定时任务 '{id}' 不存在")
        },
        stdout: String::new(),
        stderr: String::new(),
        exit_code: if response.deleted { 0 } else { 1 },
        execution: None,
    })
}

fn handle_trigger_job(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let id = get_str_field(&args, "id").ok_or_else(|| plugin_err("缺少必填参数 id"))?;
    let request = TriggerJobRequest { id };
    let response = sidecar_client::invoke::<TriggerJob>(&request)
        .map_err(|e| plugin_err(format!("trigger_job 执行失败: {e}")))?;
    let output = serde_json::to_string_pretty(&serde_json::json!({
        "id": response.job.id,
        "name": response.job.name,
        "status": "triggered"
    }))
    .unwrap_or_default();
    Ok(ToolResult {
        ok: true,
        summary: format!(
            "定时任务 '{}' 已触发并开始执行（异步，结果见执行历史）",
            response.job.name
        ),
        stdout: output,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

fn handle_get_job_runs(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let id = get_str_field(&args, "id").ok_or_else(|| plugin_err("缺少必填参数 id"))?;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
    let request = GetJobRunsRequest { id, limit };
    let response = sidecar_client::invoke::<GetJobRuns>(&request)
        .map_err(|e| plugin_err(format!("get_job_runs 执行失败: {e}")))?;
    let summary: Vec<Value> = response
        .runs
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "status": run_status_str(r),
                "started_at": r.started_at,
                "finished_at": r.finished_at,
                "result_summary": r.result_summary,
            })
        })
        .collect();
    let count = summary.len();
    Ok(ToolResult {
        ok: true,
        summary: format!("共 {count} 条执行记录"),
        stdout: serde_json::to_string_pretty(&summary).unwrap_or_default(),
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

// ── 辅助函数 ────────────────────────────────────────────────────

fn schema_create_job() -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "任务名称" },
            "description": { "type": "string", "description": "任务描述" },
            "schedule": { "type": "string", "description": "Cron 表达式（6 字段：秒 分 时 日 月 周），如 '0 0 9 * * *' 表示每天 9 点" },
            "payload": { "type": "string", "description": "触发时发送给 LLM 的任务描述" },
            "session_id": { "type": "string", "description": "关联已有会话 ID（可选，不指定则首次触发时自动创建新会话）" },
            "enabled": { "type": "boolean", "description": "是否启用，默认 true" }
        },
        "required": ["name", "description", "schedule", "payload"]
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

fn schema_update_job() -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "任务 ID" },
            "name": { "type": "string", "description": "任务名称" },
            "description": { "type": "string", "description": "任务描述" },
            "schedule": { "type": "string", "description": "Cron 表达式（6 字段：秒 分 时 日 月 周）" },
            "payload": { "type": "string", "description": "触发时发送给 LLM 的任务描述" },
            "session_id": { "type": "string", "description": "关联会话 ID" },
            "enabled": { "type": "boolean", "description": "是否启用" }
        },
        "required": ["id"]
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// 取必填字符串字段（空串/缺失返回 None）。
fn get_str_field(args: &Value, key: &str) -> Option<String> {
    let s = args.get(key)?.as_str()?;
    (!s.is_empty()).then(|| s.to_string())
}

/// 取可选字符串字段（空串/缺失/null 返回 None）。
fn get_opt_str_field(args: &Value, key: &str) -> Option<String> {
    let v = args.get(key)?;
    if v.is_null() {
        return None;
    }
    let s = v.as_str()?;
    (!s.is_empty()).then(|| s.to_string())
}

/// 取可更新的字符串字段的三态语义：
/// - None：未传/null → 不更新
/// - Some(None)：空串 → 清空
/// - Some(Some(s))：非空串 → 更新为新值
fn get_nullable_str_field(args: &Value, key: &str) -> Option<Option<String>> {
    let v = args.get(key)?;
    if v.is_null() {
        return None;
    }
    let s = v.as_str()?;
    if s.is_empty() {
        Some(None)
    } else {
        Some(Some(s.to_string()))
    }
}

fn run_status_str(run: &JobRun) -> &'static str {
    use tiangong_plugin_scheduler_protocol::JobRunStatus;
    match run.status {
        JobRunStatus::Running => "running",
        JobRunStatus::Succeeded => "succeeded",
        JobRunStatus::Failed => "failed",
    }
}

fn job_summary(job: &Job) -> Value {
    serde_json::json!({
        "id": job.id,
        "name": job.name,
        "schedule": job.schedule,
        "session_id": job.session_id,
        "enabled": job.enabled,
    })
}

// ── UI 能力（plugin-ui 接口）──

impl UiGuest for Component {
    /// 设置页（app/scheduler.html）与定时任务消息卡片（app/scheduled-task.html）
    /// 由 plugin.json 的 `ui.contributions` 声明：schema v2 插件的 WASM 贡献
    /// 不会映射到设置页 Slot。
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(Vec::new())
    }

    fn open_view(_contribution_id: String) -> Result<ViewResponse, PluginError> {
        Err(plugin_err("Scheduler 页面由 plugin.json 声明"))
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("Scheduler 页面无外部资源"))
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        let payload = match request.method.as_str() {
            "__tiangong.mention_candidates.v1" => serde_json::to_string(&[serde_json::json!({
                "value": "@plugin:scheduler",
                "label": "定时任务",
                "kind": "plugin",
                "hint": "创建、管理定时任务（Cron 调度）",
                "mark": "",
            })])
            .map_err(|e| plugin_err(e.to_string()))?,
            "list" => invoke_for_ui::<ListJobs>(&Empty {})?,
            "create" => {
                let req: CreateJobRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析创建任务请求失败: {e}")))?;
                invoke_for_ui::<CreateJob>(&req)?
            }
            "update" => {
                let req: UpdateJobRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析更新任务请求失败: {e}")))?;
                invoke_for_ui::<UpdateJob>(&req)?
            }
            "delete" => {
                let req: DeleteJobRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析删除任务请求失败: {e}")))?;
                invoke_for_ui::<DeleteJob>(&req)?
            }
            "trigger" => {
                let req: TriggerJobRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析触发任务请求失败: {e}")))?;
                invoke_for_ui::<TriggerJob>(&req)?
            }
            "runs" => {
                let req: GetJobRunsRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析执行历史请求失败: {e}")))?;
                invoke_for_ui::<GetJobRuns>(&req)?
            }
            other => return Err(plugin_err(format!("未知的定时任务管理消息: {other}"))),
        };
        Ok(ViewMessageResponse { payload })
    }
}

/// 通用 sidecar 转发器：调用操作 O 并把响应序列化成 JSON 字符串（供 iframe 消费）。
fn invoke_for_ui<O>(request: &O::Request) -> Result<String, PluginError>
where
    O: SchedulerOperation,
    O::Response: serde::Serialize,
{
    let response = sidecar_client::invoke::<O>(request).map_err(|e| plugin_err(e.to_string()))?;
    serde_json::to_string(&response).map_err(|e| plugin_err(e.to_string()))
}
bindings::export!(Component with_types_in bindings);
