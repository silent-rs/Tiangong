//! 中枢模式（central mode）宿主支持：中枢对话的独立执行域。
//!
//! **中枢对话直接由 TiangongCore 承载，不经 CoreManager**：中枢需要单独
//! 内置且唯一的中枢插件（[`CentralPlugin`]，承载中枢助手人设与 §5.3 白
//! 名单工具集），CoreManager 的插件构造（runtime 聚合桥）与编排不适用；
//! 中枢 Core 由 `TiangongApp` 直接构造、持有与投递（见 `app.rs` 的
//! `ensure_central_core` 等方法）。
//!
//! 其余边界不变：
//! - **单独存储**：路径规则内建于 `tiangong_core::session::Session::
//!   session_file_path`（`{storage_root}/central/sessions/`，读 / 写 /
//!   元数据共用）；会话列表与按组删除只扫 `sessions/`，天然不触达；
//! - **固定 Session ID**：常量位于 `tiangong_types::central_session`
//!   （前端 `frontend/src/lib/centralSession.ts` 同名常量）。

use std::future::Future;
use std::pin::Pin;

use tiangong_core::core::Plugin;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::{
    PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider,
};
use tiangong_core::tools::result::ToolResult;
use tiangong_llm::tool::{ToolCall, ToolSpec};

pub use tiangong_types::central_session::{is_central_session, CENTRAL_SESSION_ID};

/// 中枢助手人设（system prompt 段落，设计稿 §5.1 / §5.2 的 POC 收敛版）。
///
/// 中枢是控制面与交互面：理解目标、澄清需求、组织回答与计划；不直接
/// 执行工作型任务。POC 提供三个只读上下文工具（§5.3 白名单前三项），
/// 中枢应**主动使用**它们了解情况，而不是让用户提供信息。
const CENTRAL_ASSISTANT_PROMPT: &str = "\
你是天工中枢——用户的个人超级智能助手，是所有工作的统一入口与协调者。

你的职责：
- 理解用户的目标与约束，必要时澄清，给出简洁的计划或建议；
- 组织回答与汇总，明确区分「已确认的事实」「你的推测」「你的建议」；
- 用户提到天工自身、开发仓库、工作区或后台工作时，**先调用工具了解情况再回答**
  （`central__get_context`：中枢与天工的当前状态；`central__list_workspaces`：
  本机已知工作区清单及各 git 分支——天工开发仓库通常在这里能找到；
  `central__inspect_workspace`：指定工作区的只读元信息——worktree 清单等），
  不要让用户粘贴你可以自己查到的信息。

你的边界（由系统强制，不依赖你的自觉）：
- 你不直接执行工作型任务：不读写项目文件内容、不运行命令、不操作浏览器
  或数据库；你能做的读取仅限上述工具提供的元信息；
- 深入分析代码、修改文件、运行测试等实际工作由后台工作会话执行：
  用户表达此类目标（包括「让某模型做某事」）时，用 `central__dispatch_work`
  派发（模型 key 用 `central__list_models` 确认），派发后告知用户已安排；
  结果以「[后台工作完成]」消息回到本对话，届时向用户**完整转述关键结论
  与依据**（不要过度省略，重要数据/文件/命令逐条保留；用户要细节时指引
  查看工作会话）；
  不要代替用户虚构执行结果；
- 回答使用中文，保持简洁。";

/// 中枢派发后台工作的请求（中枢插件 → app 执行器，经 unbounded channel）。
///
/// 插件侧只做校验与登记（同步返回工具结果），实际创建 Worker 会话与
/// 投递由 app 侧异步执行（`TiangungApp` 的中枢派发执行器，见 app.rs）；
/// 完成或失败以注入消息回到中枢对话（事件驱动汇报的最小闭环）。
#[derive(Debug, Clone)]
pub struct CentralDispatchRequest {
    /// 派发标识（中枢对话内引用用）。
    pub work_id: String,
    /// Worker 会话 ID（插件侧生成，scru128）。
    pub session_id: String,
    /// 任务描述（原文，作为 Worker 的执行指令）。
    pub task: String,
    /// 指定模型 key（已通过解析校验；None 用路由默认）。
    pub model: Option<String>,
    /// 工作目录（None 用全局默认工作区）。
    pub cwd: Option<String>,
}

/// 中枢助手内置插件：中枢 Core 唯一挂载的插件。
///
/// POC 工具集（§5.3 白名单）：
/// - 只读上下文：`get_context` / `list_workspaces` / `inspect_workspace`
///   / `list_models`（可用模型清单）；
/// - 后台工作：`dispatch_work`（指定模型派发 Worker 会话执行任务）、
///   `list_work_items`（派发登记表）。
///
/// 其余白名单工具（暂停/恢复/取消、征询、通知等）随中枢编排后续片扩展。
pub struct CentralPlugin {
    /// 全局默认工作区（构造时注入的快照；中枢不绑定具体工作区，§6）。
    workspace_dir: String,
    /// 只读元数据源（工作区聚合）：与普通会话管理器共享同一实例，仅使用
    /// `list_session_metadata`（带缓存），不触碰会话内容或 Core 生命周期。
    sessions: tiangong_core_manager::CoreManager,
    /// 派发通道：把后台工作请求交给 app 执行器（见 app.rs）。
    dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralDispatchRequest>,
    /// 派发登记表（work_id → 派发事实；状态汇报经注入消息回到对话）。
    work_items: std::sync::Mutex<Vec<WorkItemRecord>>,
}

/// 派发登记（中枢视角的最低事实：派发了什么、给谁、什么模型）。
#[derive(Debug, Clone)]
pub struct WorkItemRecord {
    pub work_id: String,
    pub session_id: String,
    pub task: String,
    pub model: Option<String>,
    pub dispatched_at: String,
}

impl CentralPlugin {
    pub fn new(
        workspace_dir: String,
        sessions: tiangong_core_manager::CoreManager,
        dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralDispatchRequest>,
    ) -> Self {
        Self {
            workspace_dir,
            sessions,
            dispatch_tx,
            work_items: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn text_result(&self, summary: &str, body: String) -> Option<ToolResult> {
        Some(ToolResult {
            ok: true,
            summary: summary.to_string(),
            stdout: body,
            stderr: String::new(),
            exit_code: 0,
            error_kind: None,
            retryable: None,
            execution: None,
        })
    }

    fn error_result(&self, message: String) -> Option<ToolResult> {
        Some(ToolResult {
            ok: false,
            summary: message.clone(),
            stdout: String::new(),
            stderr: message,
            exit_code: 1,
            error_kind: Some("argument_error".to_string()),
            retryable: Some(false),
            execution: None,
        })
    }

    fn handle_get_context(&self) -> Option<ToolResult> {
        let workspace_count = self.known_workspace_paths().len();
        let body = format!(
            "天工版本: {version}\n\
             中枢模式: POC 阶段 1（单一中枢对话 + 指定模型派发后台工作已接入）\n\
             身份边界: 中枢是控制面，不直接执行工作型任务（§5.2）\n\
             可用工具: get_context / list_workspaces / inspect_workspace（均为只读元信息）\n\
             后台工作派发: 已接入（dispatch_work 指定模型派发后台会话执行，完成自动回报本对话）\n\
             默认工作区: {workspace}\n\
             已知工作区: {count} 个（list_workspaces 查看路径与 git 分支）\n\
             当前时间: {now}",
            version = env!("CARGO_PKG_VERSION"),
            workspace = self.workspace_dir,
            count = workspace_count,
            now = chrono::Local::now().format("%Y-%m-%d %H:%M"),
        );
        self.text_result("中枢上下文", body)
    }

    /// 聚合本机已知工作区路径：全局默认 + 各会话工作目录（元数据 cwd 去重）。
    fn known_workspace_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = vec![self.workspace_dir.clone()];
        let mut seen: std::collections::HashSet<String> = paths.iter().cloned().collect();
        for cwd in self
            .sessions
            .list_session_metadata()
            .into_iter()
            .map(|meta| meta.cwd.trim().to_string())
        {
            if cwd.is_empty() || !seen.insert(cwd.clone()) {
                continue;
            }
            paths.push(cwd);
        }
        paths
    }

    fn handle_list_workspaces(&self) -> Option<ToolResult> {
        let mut lines =
            vec!["本机已知工作区（全局默认 + 各会话工作目录聚合；元信息只读）：".to_string()];
        for path in self.known_workspace_paths() {
            let is_default = path == self.workspace_dir;
            let git_info = workspace_git_summary(std::path::Path::new(&path));
            lines.push(format!(
                "- {path}{default_tag}{git_info}",
                default_tag = if is_default { "（默认）" } else { "" },
            ));
        }
        self.text_result("工作区清单", lines.join("\n"))
    }

    /// 工作区只读元信息：存在性、git 当前分支、worktree 清单。
    ///
    /// 只读取 `.git` 元数据文件（HEAD / gitdir），不读取项目文件内容、
    /// 不执行 git 命令——属 §5.3 `inspect_workspace` 的白名单范围。
    fn handle_inspect_workspace(&self, path: &str) -> Option<ToolResult> {
        let target = if path.trim().is_empty() {
            std::path::PathBuf::from(&self.workspace_dir)
        } else {
            std::path::PathBuf::from(path.trim())
        };
        if !target.is_dir() {
            return self.error_result(format!("路径不存在或不是目录：{}", target.display()));
        }
        let mut lines = vec![format!("路径: {}", target.display())];
        let dot_git = target.join(".git");
        if !dot_git.exists() {
            lines.push("git: 非 git 仓库".to_string());
        } else if dot_git.is_file() {
            // worktree：.git 文件指向主仓库的 worktree 元目录
            let content = std::fs::read_to_string(&dot_git).unwrap_or_default();
            lines.push(format!("git: worktree（{}", content.trim()));
            // 主仓库路径：gitdir 指向 {main}/.git/worktrees/{name}，回溯两级到 .git
            if let Some(rest) = content.strip_prefix("gitdir:") {
                let meta = std::path::Path::new(rest.trim());
                if let Some(main_git) = meta.ancestors().nth(2) {
                    if let Some(main_root) = main_git.parent() {
                        lines.push(format!("主仓库: {}", main_root.display()));
                        if let Some(branch) = read_head_branch(main_git) {
                            lines.push(format!("主仓库分支: {branch}"));
                        }
                        lines.extend(list_worktrees(main_git));
                    }
                }
            }
            lines.push(")".to_string());
        } else {
            match read_head_branch(&dot_git) {
                Some(branch) => lines.push(format!("git: 主仓库，当前分支 {branch}")),
                None => lines.push("git: 仓库（HEAD 无法解析）".to_string()),
            }
            lines.extend(list_worktrees(&dot_git));
        }
        self.text_result("工作区元信息", lines.join("\n"))
    }

    /// 可用模型清单（白名单 §5.3 `list_model_profiles` 的只读版）。
    fn handle_list_models(&self) -> Option<ToolResult> {
        let models = tiangong_config::io::load_models_config_at(self.sessions.storage_root());
        let chat_default = models
            .routing
            .get(&tiangong_llm::models_config::RoutingSlot::Chat)
            .map(|entry| entry.model.clone());
        let mut lines = Vec::new();
        for (key, entry) in &models.models {
            let tag = if Some(&entry.model) == chat_default.as_ref() {
                "（默认）"
            } else {
                ""
            };
            lines.push(format!(
                "- {key}{tag}｜provider {} / model {}",
                entry.provider, entry.model
            ));
        }
        if lines.is_empty() {
            lines.push("（未配置任何模型）".to_string());
        }
        self.text_result("可用模型", lines.join("\n"))
    }

    /// 派发后台工作：校验模型 → 登记 → 经通道交给 app 执行器。
    fn handle_dispatch_work(
        &self,
        task: &str,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Option<ToolResult> {
        let task = task.trim();
        if task.is_empty() {
            return self.error_result("task 不能为空".to_string());
        }
        // 模型校验（白名单 §5.3：派发前确认模型可用；失败时附可用清单）
        if let Some(model) = model.filter(|key| !key.trim().is_empty()) {
            if let Err(error) = self.sessions.resolve_turn_model(Some(model.trim())) {
                let models =
                    tiangong_config::io::load_models_config_at(self.sessions.storage_root());
                let available: Vec<String> = models.models.keys().cloned().collect();
                return self.error_result(format!("{error}\n可用模型：{}", available.join("、")));
            }
        }
        let record = WorkItemRecord {
            work_id: format!("work-{}", &scru128::new().to_string()[..8]),
            session_id: scru128::new().to_string(),
            task: task.to_string(),
            model: model
                .filter(|key| !key.trim().is_empty())
                .map(str::trim)
                .map(str::to_string),
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        };
        let request = CentralDispatchRequest {
            work_id: record.work_id.clone(),
            session_id: record.session_id.clone(),
            task: record.task.clone(),
            model: record.model.clone(),
            cwd: cwd
                .filter(|path| !path.trim().is_empty())
                .map(str::trim)
                .map(str::to_string),
        };
        if let Err(error) = self.dispatch_tx.send(request) {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        self.work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record.clone());
        let model_note = record
            .model
            .as_deref()
            .unwrap_or("路由默认模型")
            .to_string();
        let body = format!(
            "已派发后台工作：\n- work_id: {work_id}\n- 执行模型: {model_note}\n\
             - 工作会话: {session_id}（普通模式会话列表可查看）\n- 任务: {task}\n\n\
             执行完成后结果会自动出现在本对话（以「[后台工作完成]」消息开始），\
             由你向用户转述要点。执行中如需查看可调 list_work_items。",
            work_id = record.work_id,
            model_note = model_note,
            session_id = record.session_id,
            task = record.task,
        );
        self.text_result("已派发", body)
    }

    /// 派发登记表（中枢视角的派发事实）。
    fn handle_list_work_items(&self) -> Option<ToolResult> {
        let items = self
            .work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if items.is_empty() {
            return self.text_result("后台工作", "当前没有派发记录。".to_string());
        }
        let lines: Vec<String> = items
            .iter()
            .map(|item| {
                format!(
                    "- {}｜模型 {}｜会话 {}｜派发于 {}\n  任务: {}",
                    item.work_id,
                    item.model.as_deref().unwrap_or("（默认）"),
                    item.session_id,
                    item.dispatched_at,
                    item.task
                )
            })
            .collect();
        self.text_result("后台工作", lines.join("\n"))
    }
}

/// 工作区的 git 概要（一行）：主仓库当前分支或非 git 标记。
fn workspace_git_summary(path: &std::path::Path) -> String {
    if !path.join(".git").exists() {
        return "｜非 git 仓库".to_string();
    }
    let git_dir = if path.join(".git").is_file() {
        // worktree：回溯到主仓库 .git
        std::fs::read_to_string(path.join(".git"))
            .ok()
            .and_then(|content| {
                let meta = std::path::PathBuf::from(content.trim());
                meta.ancestors().nth(2).map(|p| p.to_path_buf())
            })
    } else {
        Some(path.join(".git"))
    };
    match git_dir.as_deref().and_then(read_head_branch) {
        Some(branch) => format!("｜git 分支 {branch}"),
        None => "｜git 仓库（HEAD 无法解析）".to_string(),
    }
}

/// 读取 git 仓库 HEAD 的当前分支名（`ref: refs/heads/<name>`）。
fn read_head_branch(git_dir: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.strip_prefix("ref: refs/heads/")
        .map(str::trim)
        .map(str::to_string)
}

/// 列出 `.git/worktrees/*`：各 worktree 的位置与当前分支。
fn list_worktrees(git_dir: &std::path::Path) -> Vec<String> {
    let root = git_dir.join("worktrees");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let gitdir_file = entry.path().join("gitdir");
            let location = std::fs::read_to_string(&gitdir_file)
                .ok()
                .map(|content| {
                    std::path::Path::new(content.trim())
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_else(|| "(位置未知)".to_string());
            let branch =
                read_head_branch(&entry.path()).unwrap_or_else(|| "( detached )".to_string());
            out.push(format!("worktree {name}: {location}（分支 {branch}）"));
        }
    }
    if out.is_empty() {
        out.push("worktrees: 无".to_string());
    } else {
        out.insert(0, "worktrees:".to_string());
    }
    out
}

impl Plugin for CentralPlugin {
    fn id(&self) -> &str {
        "central"
    }
}

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
                name: "dispatch_work".to_string(),
                description: "派发后台工作：创建一个后台工作会话（普通能力、全插件），\
                    用指定模型执行任务，完成后结果自动回到本对话由你转述用户。\
                    用户说「让某模型做某事」「去分析/审查/整理某工作区」时使用。"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "给执行会话的任务描述（完整、自包含）"},
                        "model": {"type": "string", "description": "模型 key（list_models 查询；省略用路由默认）"},
                        "cwd": {"type": "string", "description": "工作目录（省略用默认工作区）"}
                    },
                    "required": ["task"]
                }),
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

impl ToolOverrideHandler for CentralPlugin {
    fn handle(
        &self,
        call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        let result = match call.name.as_str() {
            "get_context" => self.handle_get_context(),
            "list_workspaces" => self.handle_list_workspaces(),
            "inspect_workspace" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_inspect_workspace(path)
            }
            "list_models" => self.handle_list_models(),
            "dispatch_work" => {
                let task = call
                    .arguments
                    .get("task")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let model = call.arguments.get("model").and_then(|value| value.as_str());
                let cwd = call.arguments.get("cwd").and_then(|value| value.as_str());
                self.handle_dispatch_work(task, model, cwd)
            }
            "list_work_items" => self.handle_list_work_items(),
            _ => None,
        };
        Box::pin(async move { result })
    }
}

impl PromptSectionProvider for CentralPlugin {
    fn prompt_sections(&self) -> Vec<String> {
        vec![CENTRAL_ASSISTANT_PROMPT.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_session_id_matches_frontend_constant() {
        // 与前端 frontend/src/lib/centralSession.ts 的 CENTRAL_SESSION_ID 一致；
        // 变更需两侧同步（历史文件按 ID 命名，改值会丢弃既有中枢对话）。
        assert_eq!(CENTRAL_SESSION_ID, "central-hub");
        assert!(is_central_session(CENTRAL_SESSION_ID));
        assert!(!is_central_session("0198abcd"));
        assert!(!is_central_session(""));
    }

    /// 中枢会话文件单独存储，且路径规则单点维护（读/写/元数据共用）。
    #[test]
    fn central_session_file_is_isolated_but_single_rule() {
        use tiangong_core::session::Session;
        let root = std::path::Path::new("/data/tiangong");
        assert_eq!(
            Session::session_file_path(root, CENTRAL_SESSION_ID).unwrap(),
            std::path::PathBuf::from("/data/tiangong/central/sessions/central-hub.json")
        );
        assert_eq!(
            Session::session_file_path(root, "0198abcd").unwrap(),
            std::path::PathBuf::from("/data/tiangong/sessions/0198abcd.json")
        );
        assert!(Session::session_file_path(root, "../evil").is_err());
        assert!(Session::session_file_path(root, "a/b").is_err());
    }

    /// 端到端读写一致性：中枢 Session 持久化到独立目录并可按同一路径加载。
    #[test]
    fn central_session_roundtrip_in_isolated_dir() {
        use tiangong_core::session::Session;
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new("中枢对话");
        session.id = CENTRAL_SESSION_ID.to_string();
        session.bind_storage_root(dir.path());
        session.try_persist_to_disk().unwrap();

        assert!(dir
            .path()
            .join("central/sessions/central-hub.json")
            .exists());
        assert!(!dir.path().join("sessions/central-hub.json").exists());

        let loaded = Session::load_from_storage(dir.path(), CENTRAL_SESSION_ID).unwrap();
        assert_eq!(loaded.id, CENTRAL_SESSION_ID);
    }

    /// 人设注入 + 白名单工具声明（§5.3 前三项）。
    /// 测试辅助：构造 plugin（派发请求收集到返回的接收端，供断言）。
    fn plugin_with_channel(
        workspace_dir: String,
        manager: tiangong_core_manager::CoreManager,
    ) -> (
        CentralPlugin,
        tokio::sync::mpsc::UnboundedReceiver<CentralDispatchRequest>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (CentralPlugin::new(workspace_dir, manager, tx), rx)
    }

    /// 测试辅助：构造带独立 sessions 目录的 manager，并写入指定 cwd 的会话元数据。
    fn manager_with_session_cwds(
        root: &std::path::Path,
        cwds: &[&str],
    ) -> tiangong_core_manager::CoreManager {
        use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        for (index, cwd) in cwds.iter().enumerate() {
            let id = format!("s{index}");
            let session = serde_json::json!({
                "id": id,
                "title": format!("会话 {index}"),
                "created_at": "2026-11-08 10:00:00",
                "updated_at": "2026-11-08 10:00:00",
                "trust_mode": "Supervised",
                "cwd": cwd,
                "messages": []
            });
            std::fs::write(sessions_dir.join(format!("{id}.json")), session.to_string()).unwrap();
        }
        tiangong_core_manager::CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            root.to_path_buf(),
        )
    }

    #[test]
    fn central_plugin_persona_and_tool_specs() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx) = plugin_with_channel("/tmp/ws".to_string(), manager);
        assert_eq!(plugin.id(), "central");
        let sections = plugin.prompt_sections();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("天工中枢"));
        assert!(sections[0].contains("先调用工具了解情况再回答"));
        let specs = plugin.tool_specs();
        let names: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
        assert_eq!(
            names,
            vec![
                "get_context".to_string(),
                "list_workspaces".to_string(),
                "inspect_workspace".to_string(),
                "list_models".to_string(),
                "dispatch_work".to_string(),
                "list_work_items".to_string(),
            ]
        );
    }

    fn call_tool(plugin: &CentralPlugin, name: &str, args: serde_json::Value) -> ToolResult {
        let call = ToolCall {
            id: "t1".to_string(),
            name: name.to_string(),
            arguments: args,
        };
        let mut session = Session::new("测试");
        let future = ToolOverrideHandler::handle(plugin, &call, &mut session, "test");
        futures_block_on(future).expect("中枢工具必须被本插件处理")
    }

    /// 极简 block_on（测试内同步等待一次性 future，不引入运行时依赖）。
    fn futures_block_on<F: Future>(future: F) -> F::Output {
        // 简单轮询：当前 future 全部同步完成（Box::pin(async move { result })），
        // poll 一次即 Ready。
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("中枢插件工具不应挂起"),
        }
    }

    #[test]
    fn get_context_reports_central_state() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(&plugin, "get_context", serde_json::json!({}));
        assert!(result.ok);
        assert!(result.stdout.contains("后台工作派发: 已接入"));
        assert!(result.stdout.contains("/tmp/ws"));
    }

    /// 派发：模型 key 校验通过后经通道发出并登记。
    #[test]
    fn dispatch_work_sends_request_and_registers() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        // 提供一个可用模型，供派发校验
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "审查仓库提交", "model": "k3", "cwd": "/tmp"}),
        );
        assert!(result.ok, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("已派发后台工作"));
        let request = dispatch_rx.try_recv().expect("派发请求应经通道发出");
        assert_eq!(request.model.as_deref(), Some("k3"));
        assert_eq!(request.task, "审查仓库提交");
        assert_eq!(request.cwd.as_deref(), Some("/tmp"));
        assert!(!request.work_id.is_empty());
        // 登记表可见
        let listing = call_tool(&plugin, "list_work_items", serde_json::json!({}));
        assert!(listing.stdout.contains(&request.work_id));
        assert!(listing.stdout.contains("审查仓库提交"));
    }

    /// 派发：模型 key 不存在时拒绝并附可用清单。
    #[test]
    fn dispatch_work_rejects_unknown_model() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "审查", "model": "not-exist"}),
        );
        assert!(!result.ok);
        assert!(result.stderr.contains("可用模型"));
        assert!(result.stderr.contains("k3"));
        assert!(dispatch_rx.try_recv().is_err(), "校验失败不得派发");
    }

    /// 模型清单：列出 key 并标记路由默认。
    #[test]
    fn list_models_reports_registry() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]},
                    "glm": {"provider": "p", "model": "glm-5.3", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(&plugin, "list_models", serde_json::json!({}));
        assert!(result.ok);
        assert!(result.stdout.contains("k3"));
        assert!(result.stdout.contains("k3-pro"));
        assert!(result.stdout.contains("（默认）"));
    }

    #[test]
    fn inspect_workspace_reads_git_metadata() {
        let main = tempfile::tempdir().unwrap();
        let git = main.path().join(".git");
        std::fs::create_dir_all(git.join("worktrees/central-poc")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/poc/central-mode\n").unwrap();
        let worktree_dir = main.path().join("wt/central-poc");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        // worktree 的 .git 是指向主仓库元目录的指针文件
        std::fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", git.join("worktrees/central-poc").display()),
        )
        .unwrap();
        std::fs::write(
            git.join("worktrees/central-poc/HEAD"),
            "ref: refs/heads/poc/central-mode\n",
        )
        .unwrap();
        std::fs::write(
            git.join("worktrees/central-poc/gitdir"),
            format!("{}/.git\n", worktree_dir.display()),
        )
        .unwrap();

        let (plugin, _dispatch_rx) = plugin_with_channel(
            main.path().display().to_string(),
            manager_with_session_cwds(main.path(), &[]),
        );
        let result = call_tool(
            &plugin,
            "inspect_workspace",
            serde_json::json!({"path": worktree_dir.display().to_string()}),
        );
        assert!(result.ok);
        assert!(result.stdout.contains("主仓库分支: poc/central-mode"));
        assert!(result.stdout.contains("worktree central-poc"));
    }

    #[test]
    fn inspect_workspace_rejects_missing_path() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "inspect_workspace",
            serde_json::json!({"path": "/definitely/not/exist"}),
        );
        assert!(!result.ok);
        assert_eq!(result.error_kind.as_deref(), Some("argument_error"));
    }
    #[test]
    fn list_workspaces_aggregates_session_cwds_with_git_branch() {
        let root = tempfile::tempdir().unwrap();
        // 两个会话 cwd，其中一个重复
        let repo = root.path().join("dev/tiangong");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/poc/central-mode\n").unwrap();
        let manager = manager_with_session_cwds(
            root.path(),
            &[repo.to_str().unwrap(), repo.to_str().unwrap()],
        );
        let (plugin, _dispatch_rx) =
            plugin_with_channel(root.path().display().to_string(), manager);
        let result = call_tool(&plugin, "list_workspaces", serde_json::json!({}));
        assert!(result.ok);
        // 默认工作区在列（去重后仅一次 dev/tiangong）
        assert!(result.stdout.contains("（默认）"));
        assert_eq!(result.stdout.matches("dev/tiangong").count(), 1);
        assert!(result.stdout.contains("git 分支 poc/central-mode"));
    }
}
