//! 中枢助手工具 schema（R5 从 lib.rs 拆出；二十轮工具合并：21 → 10）。
//!
//! 合并入口（2026-11-15 用户裁决）：
//! - `inspect_context`：环境感知统一入口（list_workspaces / inspect_workspace / list_sessions）
//! - `dispatch_work`：吸收 send_to_worker（复用在跑会话自动作为引导并入）
//! - `control`：cancel / pause / resume 统一入口
//! - `work`：工作查询统一入口（list / status / timeline / reports）
//! - `goal`：目标与报告统一入口（save_goal / set_goal_status / save_report / list_reports）
//!
//! 工具集与设计稿 §5.3 白名单的对应关系见 lib.rs 头注释。

use tiangong_core::tools::extension::ToolSpecProvider;
use tiangong_llm::tool::ToolSpec;

use crate::CentralPlugin;

impl ToolSpecProvider for CentralPlugin {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![
            // ── 上下文感知 ─────────────────────────────────────────────
            ToolSpec {
                name: "get_context".to_string(),
                description: "获取中枢与天工的当前状态：后台工作清单、待汇报条数、中断数、当前目标、默认工作区、时间。回答天工自身状态或开发情况前先调用。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "inspect_context".to_string(),
                description: "环境感知统一入口。缺省调用返回工作区+近期会话的综合摘要；focus 深入查询：focus=workspace 需带 path（该工作区 git 分支与 worktree 清单）；focus=sessions 完整会话列表（含消息数）；focus=workspaces 完整工作区列表。派发前了解环境优先用本工具。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "focus": {"type": "string", "enum": ["workspaces", "workspace", "sessions"], "description": "深入查询维度；缺省给综合摘要"},
                        "path": {"type": "string", "description": "focus=workspace 时必填：工作区绝对路径"}
                    }
                }),
            },
            ToolSpec {
                name: "list_models".to_string(),
                description: "列出可用的模型清单（key 与默认标记）。用户指定模型派发工作前先确认 key 是否存在。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            // ── 派发与控制 ─────────────────────────────────────────────
            ToolSpec {
                name: "dispatch_work".to_string(),
                description: "统一派发入口。行为按目标会话状态自动分派：新会话执行任务 / 复用空闲会话开新任务（上下文连续）/ 复用在跑会话时 task 自动作为引导消息并入当前执行（调整方向、补充要求、追问细节）。用户说「让某模型做某事」「继续上次的工作」「调整某工作」时都使用本工具。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "任务描述（新任务用完整描述；引导用简洁指令）"},
                        "session_id": {"type": "string", "description": "复用/引导的目标会话 ID（inspect_context focus=sessions 或 work action=list 查询）"},
                        "model": {"type": "string", "description": "指定模型 key（list_models 确认；缺省用路由默认）。复用会话时即换模型通道：空闲本轮生效、执行中本轮沿用当前模型"},
                        "cwd": {"type": "string", "description": "工作目录（缺省复用会话自身或全局默认；其他目录需用户授权）"},
                        "after_work_id": {"type": "string", "description": "排队依赖：等待该 work 终态后再投递本工作"},
                        "parallel": {"type": "boolean", "description": "写入锁豁免（仅用户明确要求并行且任务只读时使用）"}
                    },
                    "required": ["task"]
                }),
            },
            ToolSpec {
                name: "control".to_string(),
                description: "工作控制统一入口。action：cancel（终态，中断不再恢复，再做需重派）/ pause（中断执行，上下文保留）/ resume（发继续指令，回到执行中）。用户要求停止、暂停或继续某个工作时使用；不要用重派代替控制。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "目标工作 ID（work 或 list 查询）"},
                        "action": {"type": "string", "enum": ["cancel", "pause", "resume"]}
                    },
                    "required": ["work_id", "action"]
                }),
            },
            // ── 工作查询 ───────────────────────────────────────────────
            ToolSpec {
                name: "work".to_string(),
                description: "工作查询统一入口。action：list（缺省，已派发工作登记列表）/ status（单工作详情+时间线，需 work_id）/ timeline（完整事件时间线，需 work_id）/ reports（收集全部已到达回报——取走语义，收集后清空，在回复中统一转述）。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "status", "timeline", "reports"], "description": "缺省 list"},
                        "work_id": {"type": "string", "description": "action=status/timeline 时必填"}
                    }
                }),
            },
            // ── 目标与报告 ─────────────────────────────────────────────
            ToolSpec {
                name: "goal".to_string(),
                description: "目标与报告统一入口。action：save_goal（登记用户目标，同名幂等；title 必填）/ set_goal_status（goal_id+status 必填：achieved/abandoned/archived）/ save_report（结构化报告存档；work_id+summary 必填）/ list_reports（查询已存档报告，work_id 可选过滤）。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["save_goal", "set_goal_status", "save_report", "list_reports"]},
                        "title": {"type": "string", "description": "save_goal：目标标题"},
                        "description": {"type": "string", "description": "save_goal：目标详情"},
                        "goal_id": {"type": "string", "description": "set_goal_status：目标 ID"},
                        "status": {"type": "string", "enum": ["achieved", "abandoned", "archived"], "description": "set_goal_status：目标状态"},
                        "work_id": {"type": "string", "description": "save_report/list_reports：关联工作 ID"},
                        "summary": {"type": "string", "description": "save_report：报告结论"},
                        "artifacts": {"type": "array", "items": {"type": "string"}, "description": "save_report：产物路径列表"},
                        "risks": {"type": "array", "items": {"type": "string"}, "description": "save_report：风险与未尽事项"}
                    },
                    "required": ["action"]
                }),
            },
            // ── 授权、通知与搜索 ───────────────────────────────────────
            ToolSpec {
                name: "request_workspace_approval".to_string(),
                description: "向用户发起 Workspace 授权请求（触发授权卡片）。dispatch_work 因目录未授权被拒后使用：先请求授权并向用户说明，用户确认后重派同一任务。task_fingerprint 必须与 dispatch_work 的 task 完全一致（防偷换）。你只能请求授权，不能代替用户授权。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "要授权的工作区绝对路径"},
                        "task_fingerprint": {"type": "string", "description": "任务指纹（与 dispatch_work 的 task 文本完全一致）"}
                    },
                    "required": ["path", "task_fingerprint"]
                }),
            },
            ToolSpec {
                name: "send_notification".to_string(),
                description: "向用户发送应用内通知（桌面气泡）。仅用于需要用户即时知晓的事件（高风险操作提醒、关键完成、需尽快决策）；常规汇报走对话，不要滥用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "message": {"type": "string"},
                        "level": {"type": "string", "enum": ["info", "warning", "error"]}
                    },
                    "required": ["title"]
                }),
            },
            ToolSpec {
                name: "search_history".to_string(),
                description: "搜索历史会话消息（跨会话）：标题走元数据缓存，正文扫描近期会话（子串匹配）。用户问「之前谁说过什么/某结论在哪个会话/以前怎么处理的」时使用；也可用于派发前确认是否已有相关工作。scope=all 扫全部（较慢），默认近期会话。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "number", "description": "返回上限（默认 10）"},
                        "scope": {"type": "string", "enum": ["recent", "all"], "description": "扫描范围（默认 recent）"}
                    },
                    "required": ["query"]
                }),
            },
        ]
    }
}
