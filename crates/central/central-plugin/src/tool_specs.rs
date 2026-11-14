//! 中枢助手工具 schema（R1 从 lib.rs 拆出）：ToolSpecProvider 实现。
//!
//! 工具集与 docs/central-mode-design-draft.md §5.3 白名单对应；
//! 权限强制点在宿主路由层（不在提示词）。

use tiangong_core::tools::extension::ToolSpecProvider;
use tiangong_llm::tool::ToolSpec;

use crate::CentralPlugin;

impl ToolSpecProvider for CentralPlugin {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "get_context".to_string(),
                description: "获取中枢与天工的当前状态：版本、中枢模式阶段、能力边界、\
                    后台工作清单、默认工作区。回答天工自身或开发情况前先调用本工具。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "list_workspaces".to_string(),
                description: "列出本机已知工作区（全局默认 + 各会话工作目录聚合），\
                    附各工作区 git 分支。天工开发仓库等通常能在这里找到。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "inspect_workspace".to_string(),
                description: "读取指定工作区的只读元信息：路径、git 当前分支、\
                    worktree 清单（含各 worktree 的分支）。path 省略时检查默认工作区。\
                    仅读取元数据，不读取文件内容、不执行命令。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "工作区路径，省略用默认工作区"}
                    }
                }),
            },
            ToolSpec {
                name: "list_models".to_string(),
                description: "列出可用的模型清单（key 与默认标记）。用户指定模型派发\
                    工作前先确认模型 key 是否存在。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "list_sessions".to_string(),
                description: "列出近期会话（id/标题/消息数/更新时间/工作目录）。派发后台工作\
                    前先查询：任务与既有会话相关（同工作区、同主题、先前工作的延续）时\
                    用其 id 复用（应用尽用，上下文连续）。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "dispatch_work".to_string(),
                description: "派发后台工作：创建后台工作会话（普通能力、全插件）执行任务，\
                    或复用已有会话（session_id，应用尽用），完成后结果按时机回到本对话\
                    由你转述用户。用户说「让某模型做某事」「继续上次的某工作」时使用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "给执行会话的任务描述（完整、自包含）"},
                        "model": {"type": "string", "description": "模型 key（list_models 查询；省略用路由默认）"},
                        "cwd": {"type": "string", "description": "工作目录（省略：复用会话用其自身目录，新建用默认工作区）"},
                        "session_id": {"type": "string", "description": "复用的已有会话 ID（list_sessions 查询）。任务与某会话相关（同工作区、同主题、上次未完成）时必须优先复用，保持上下文连续；仅无相关会话时省略新建"},
                        "after_work_id": {"type": "string", "description": "排队依赖：等待该工作（work_id 或会话 ID）终态（完成/失败/取消）后再自动投递本任务"},
                        "parallel": {"type": "boolean", "description": "Workspace 写入锁豁免：仅只读任务且用户明确要求并行时 true（默认 false，同目录冲突自动排队）"}
                    },
                    "required": ["task"]
                }),
            },
            ToolSpec {
                name: "search_history".to_string(),
                description: "搜索历史会话消息（跨会话）：标题走元数据缓存，正文扫描 近期会话（子串匹配），返回命中片段。用户问「之前谁说过什么/某结论 在哪个会话/以前怎么处理的」时使用；也可用于派发前确认是否已有 相关工作。scope=all 扫全部（较慢），默认近期 200 个会话。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "关键词（子串匹配）"},
                        "limit": {"type": "number", "description": "命中条数上限（默认 10，最大 30）"},
                        "scope": {"type": "string", "enum": ["recent", "all"], "description": "扫描范围：recent=最近 200 个会话（默认），all=全部（较慢）"}
                    },
                    "required": ["query"]
                }),
            },
            ToolSpec {
                name: "get_work_timeline".to_string(),
                description: "查询后台工作的事件时间线（派发/排队/阻塞/恢复/暂停/取消/终态等\n 关键节点，持久化可回查）。用户想了解某工作经历了什么/为何中断/\n 何时完成时使用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "目标工作 ID（list_work_items 查询）"}
                    },
                    "required": ["work_id"]
                }),
            },
            ToolSpec {
                name: "create_goal".to_string(),
                description: "登记用户目标（跨工作持续存在的语义核心）。用户表达目标/意图时使用；\
                    后续派发与汇报围绕目标组织，get_context 会注入当前目标列表。同名 active\
                    目标幂等。达成或放弃时用 update_goal_status 更新。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "目标一句话标题"},
                        "description": {"type": "string", "description": "目标补充说明（背景、成功标准）"},
                        "workspace": {"type": "string", "description": "关联工作区路径（可选）"}
                    },
                    "required": ["title"]
                }),
            },
            ToolSpec {
                name: "update_goal_status".to_string(),
                description: "更新目标状态：active / achieved（达成）/ abandoned（放弃）/ archived。\
                    用户反馈目标完成或放弃时使用，保持目标列表与事实一致。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "goal_id": {"type": "string", "description": "目标 ID（create_goal 返回）"},
                        "status": {"type": "string", "enum": ["active", "achieved", "abandoned", "archived"]}
                    },
                    "required": ["goal_id", "status"]
                }),
            },
            ToolSpec {
                name: "create_report".to_string(),
                description: "结构化报告存档（工作终态的结论/产物/风险持久化）。重要工作完成或\
                    失败收尾时使用，供用户日后回查历史结论；list_reports 可查询。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "关联工作 ID"},
                        "summary": {"type": "string", "description": "结论摘要（完整关键结论）"},
                        "artifacts": {"type": "array", "items": {"type": "string"}, "description": "产物路径列表"},
                        "risks": {"type": "array", "items": {"type": "string"}, "description": "风险与未尽事项"}
                    },
                    "required": ["work_id", "summary"]
                }),
            },
            ToolSpec {
                name: "list_reports".to_string(),
                description: "查询已存档的结构化报告（可按 work_id 过滤）。用户问「之前那个任务\
                    的结论/产出了什么」时使用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "按工作过滤（可选，缺省全部）"}
                    }
                }),
            },
            ToolSpec {
                name: "get_work_item_status".to_string(),
                description: "查询单个后台工作的详情：任务、终态、事件时间线（派发/阻塞/恢复/\
                    控制/终态全程）。用户问「那个任务怎么样了/经历了什么」时使用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "目标工作 ID"}
                    },
                    "required": ["work_id"]
                }),
            },
            ToolSpec {
                name: "switch_worker_model".to_string(),
                description: "切换工作会话的模型（§15.10/15.11：换模型先商量）。必须先在对话中\
                    与用户说明原因并征得同意（当前模型不可用/质量不佳等），同意后携带\
                    consultation_summary（商量结论）调用。执行中的工作在下轮生效。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string", "description": "目标工作会话 ID"},
                        "model": {"type": "string", "description": "新模型 key（list_models 查询）"},
                        "consultation_summary": {"type": "string", "description": "与用户商量的结论（原因 + 用户同意的事实）"}
                    },
                    "required": ["session_id", "model", "consultation_summary"]
                }),
            },
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
                description: "向用户发送应用内通知（桌面气泡）。重要事件需要用户即时知晓\
                    （高风险操作提醒、关键完成、需要尽快决策）时使用；常规汇报走对话，\
                    不要滥用通知。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "通知标题"},
                        "message": {"type": "string", "description": "通知正文"},
                        "level": {"type": "string", "enum": ["info", "warning", "error"], "description": "级别（默认 info）"}
                    },
                    "required": ["title"]
                }),
            },
            ToolSpec {
                name: "cancel_work".to_string(),
                description: "取消后台工作：中断该工作会话的当前执行并标记为已取消（终态，不再恢复）。\n 用户明确要求停止/放弃某个工作时使用；取消后如需再做需重新派发。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "目标工作 ID（list_work_items 查询）"}
                    },
                    "required": ["work_id"]
                }),
            },
            ToolSpec {
                name: "pause_work".to_string(),
                description: "暂停后台工作：中断当前执行但保留全部上下文（会话不删除），状态标记为已暂停。\n 用户要求暂停/先放一放时使用；恢复用 resume_work。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "目标工作 ID（list_work_items 查询）"}
                    },
                    "required": ["work_id"]
                }),
            },
            ToolSpec {
                name: "resume_work".to_string(),
                description: "恢复已暂停的后台工作：向该工作会话发送「继续之前暂停的任务」指令，\n 工作回到执行中（上下文连续）。用户要求继续/恢复时使用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "work_id": {"type": "string", "description": "目标工作 ID（list_work_items 查询）"}
                    },
                    "required": ["work_id"]
                }),
            },
            ToolSpec {
                name: "send_to_worker".to_string(),
                description: "向已派发的后台工作会话发送引导消息：调整方向、补充要求、\
                    追问细节。会话执行中作为引导消息并入当前任务；空闲时开启新任务。\
                    完成后结果同样回到本对话。用户要求「调整/补充/追问某个后台工作」时\
                    使用，不要为调整而重新派发。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string", "description": "目标工作会话 ID（list_work_items / list_sessions 查询）"},
                        "message": {"type": "string", "description": "引导消息内容（完整、明确的调整或补充指令）"}
                    },
                    "required": ["session_id", "message"]
                }),
            },
            ToolSpec {
                name: "collect_work_reports".to_string(),
                description: "收集全部已到达的后台工作回报（取走语义）。用户询问后台工作结果/进展时、或 get_context 显示有待汇报条目时调用，在当前回复中统一转述。".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "list_work_items".to_string(),
                description: "列出已派发的后台工作登记（work_id、模型、会话、任务、时间）。"
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
        ]
    }
}
