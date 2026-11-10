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
- **应用尽用（硬规则）**：用户的请求是对**先前工作的继续/追加/复查/重做**
  （如「继续 review」「再看看」「接着做」「上次的任务再加一条」）时，
  **必须复用先前派发的工作会话**——先从本对话历史或
  `central__list_work_items` 找到对应 session_id，`dispatch_work` 带
  session_id 派发。复用的收益：执行会话保留上轮全部上下文（基线、已发现
  问题、结论），无需你在任务里复述背景，增量判断更准。仅当确认是**全新
  工作**（与任何既有会话都无关）时才省略 session_id 新建；不确定时先
  `central__list_sessions` 查证再决定；
- **调整用引导，不用重派**：用户（或你自己发现）需要调整进行中/已完成
  工作的方向、补充要求、追问细节时，用 `central__send_to_worker` 向
  对应工作会话发引导消息（上下文与结果连续），不要重新派发一个新工作；
  后台工作回报的呈现时机由系统控制（失败立即单独汇报；成功且仍有
  任务在执行时暂存；全部结束后触发一次汇总轮）。你在汇报/汇总轮里的
  职责是**区分轻重**：关键结论与依据完整呈现（不要过度省略，重要
  数据/文件/命令逐条保留），琐碎进展合并一句带过即可，不要逐条等长
  展开；用户主动询问结果或 get_context 显示有待汇报条目时，调
  `central__collect_work_reports` 收集后统一转述；用户要细节时指引
  查看工作会话；
  不要代替用户虚构执行结果；
- 回答使用中文，保持简洁。";

/// 后台工作状态登记条目（前端「后台工作」状态条数据源，命令
/// `get_central_work_status` 返回）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CentralWorkStatus {
    pub work_id: String,
    pub session_id: String,
    pub task: String,
    pub model: Option<String>,
    pub dispatched_at: String,
    /// `running`（在执行）/ `report_queued`（已完成，回报暂存待汇报）。
    pub state: String,
    /// 是否复用了已有会话（应用尽用）。
    pub reused_session: bool,
}

/// 中枢插件 → app 执行器的消息（unbounded channel）。
#[derive(Debug, Clone)]
pub enum CentralPluginMessage {
    /// 派发后台工作（见 [`CentralDispatchRequest`]）。
    Dispatch(CentralDispatchRequest),
    /// 向已有 Worker 会话发送引导消息（见 [`CentralDispatchRequest`]，
    /// `task` 为引导内容；执行中作为引导消息并入当前 turn，空闲时开启
    /// 新 turn——完成后与派发同样走回报时机规则）。
    Steer(CentralDispatchRequest),
    /// `collect_work_reports` 已取走全部暂存回报：同步清理状态登记。
    ReportsCollected,
}

/// 中枢派发后台工作的请求。
///
/// 插件侧只做校验与登记（同步返回工具结果），实际创建/复用 Worker 会话
/// 与投递由 app 侧异步执行（`TiangungApp` 的中枢派发执行器，见 app.rs）；
/// 完成或失败按时机规则回到中枢对话（§9：失败必达、成功汇总）。
#[derive(Debug, Clone)]
pub struct CentralDispatchRequest {
    /// 派发标识（中枢对话内引用用）。
    pub work_id: String,
    /// Worker 会话 ID：新建时为插件生成的 scru128；复用已有会话时为
    /// `reuse_session_id` 校验通过后的同一 ID（应用尽用，保持上下文连续）。
    pub session_id: String,
    /// 任务描述（原文，作为 Worker 的执行指令）。
    pub task: String,
    /// 指定模型 key（已通过解析校验；None 用路由默认）。
    pub model: Option<String>,
    /// 工作目录（None：复用会话时用该会话自身 cwd，新建用全局默认）。
    pub cwd: Option<String>,
    /// 是否复用已有会话（插件侧决定并携带；app 据此跳过新会话标题等）。
    pub reused_session: bool,
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
    dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralPluginMessage>,
    /// 已到达、尚未进入对话的后台工作回报（与 app 共享同一实例；
    /// 暂存批量模式：随用户下一条消息一并注入，或经 collect_work_reports
    /// 由中枢在当前 turn 主动收集）。
    work_reports: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// 派发登记表（work_id → 派发事实；状态汇报经注入消息回到对话）。
    /// 持久化于 `{storage_root}/central/work-items.json`：应用重启后
    /// `list_work_items` 仍可查到既往派发（应用尽用的判定依据之一）。
    work_items: std::sync::Mutex<Vec<WorkItemRecord>>,
}

/// 派发登记（中枢视角的最低事实：派发了什么、给谁、什么模型）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkItemRecord {
    pub work_id: String,
    /// 是否复用了已有会话（应用尽用；true 时 session_id 是既有会话）。
    #[serde(default)]
    pub reused_session: bool,
    pub session_id: String,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
    pub dispatched_at: String,
    /// 终态时间（完成/失败到达时刻）；None = 未终态——应用重启后仍为
    /// None 的近期条目按「已中断」恢复到状态登记（§15.1 离开期间变化）。
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// 派发登记持久化路径：`{storage_root}/central/work-items.json`。
fn work_items_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("work-items.json")
}

/// 加载派发登记（损坏/缺失按空表处理）。
/// 加载派发登记（损坏/缺失按空表处理）。
fn load_work_items(storage_root: &std::path::Path) -> Vec<WorkItemRecord> {
    std::fs::read_to_string(work_items_path(storage_root))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// 全表落盘（原子写：tmp + rename）。
fn save_work_items(storage_root: &std::path::Path, items: &[WorkItemRecord]) {
    let Ok(content) = serde_json::to_string_pretty(items) else {
        return;
    };
    let path = work_items_path(storage_root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = path.with_extension("json.tmp");
    if std::fs::write(&tmp_path, &content).is_ok() {
        let _ = std::fs::rename(&tmp_path, &path);
    }
}

/// 更新某条登记的终态时间并落盘。
pub fn mark_work_item_finished(storage_root: &std::path::Path, work_id: &str) {
    let mut items = load_work_items(storage_root);
    if let Some(item) = items.iter_mut().find(|item| item.work_id == work_id) {
        item.finished_at = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
    } else {
        return;
    }
    save_work_items(storage_root, &items);
}

/// 近期（48h）派发且未终态的登记——应用重启后按「已中断」恢复。
pub fn interrupted_work_items(storage_root: &std::path::Path) -> Vec<WorkItemRecord> {
    // dispatched_at 为「%Y-%m-%d %H:%M:%S」本地时间，字典序与时间序一致，
    // 字符串比较近似近 48 小时窗口。
    let cutoff = (chrono::Local::now() - chrono::Duration::hours(48))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    load_work_items(storage_root)
        .into_iter()
        .filter(|item| item.finished_at.is_none() && item.dispatched_at.as_str() > cutoff.as_str())
        .collect()
}

/// 会话级 upsert 派发登记并落盘（同会话更新、保留最近 20 条）。
fn upsert_work_item(storage_root: &std::path::Path, record: &WorkItemRecord) {
    let mut items = load_work_items(storage_root);
    // 会话维度记忆：同一会话的再次派发/引导更新原记录（反映该会话
    // 最近一次工作），不堆积多条；尾部为最新。
    if let Some(idx) = items
        .iter()
        .position(|item| item.session_id == record.session_id)
    {
        items.remove(idx);
    }
    items.push(record.clone());
    // 保留上限：登记表只服务「近期派发会话的记忆与复用」（按用户
    // 要求保留 20 个会话条目）；会话文件本身不受影响——那是复用与
    // 复核的真相源。
    const MAX_WORK_ITEMS: usize = 20;
    if items.len() > MAX_WORK_ITEMS {
        let overflow = items.len() - MAX_WORK_ITEMS;
        items.drain(..overflow);
    }
    if let Some(parent) = work_items_path(storage_root).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    save_work_items(storage_root, &items);
}

impl CentralPlugin {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_dir: String,
        sessions: tiangong_core_manager::CoreManager,
        dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralPluginMessage>,
        work_reports: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        let stored_work_items = load_work_items(sessions.storage_root());
        Self {
            workspace_dir,
            sessions,
            dispatch_tx,
            work_reports,
            work_items: std::sync::Mutex::new(stored_work_items),
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
        let pending_reports = self
            .work_reports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let interrupted_reports = interrupted_work_items(self.sessions.storage_root()).len();
        let body = format!(
            "天工版本: {version}\n\
             中枢模式: POC 阶段 1（单一中枢对话 + 指定模型派发后台工作已接入）\n\
             身份边界: 中枢是控制面，不直接执行工作型任务（§5.2）\n\
             可用工具: get_context / list_workspaces / inspect_workspace / list_models / list_sessions / dispatch_work / send_to_worker / collect_work_reports / list_work_items / search_history（查询类只读；dispatch_work 派发、支持 session_id 复用；send_to_worker 发引导；search_history 跨会话搜消息）\n\
             后台工作派发: 已接入（dispatch_work 指定模型派发后台会话执行，完成进入待汇报队列，随用户下一条消息一并进入对话，或由你调 collect_work_reports 主动收集）\n\
             默认工作区: {workspace}\n\
             已知工作区: {count} 个（list_workspaces 查看路径与 git 分支）\n\
             待汇报的后台工作回报: {pending} 条（可调 collect_work_reports 收集后转述）\n\
             因应用重启中断的任务: {interrupted} 个（徽章可见；结果未产出，可重派复用原会话）\n\
             当前时间: {now}",
            version = env!("CARGO_PKG_VERSION"),
            workspace = self.workspace_dir,
            count = workspace_count,
            pending = pending_reports,
            interrupted = interrupted_reports,
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

    /// 派发后台工作：校验模型与复用会话 → 登记 → 经通道交给 app 执行器。
    ///
    /// `session_id` 提供时复用该已有会话（应用尽用：任务与既有会话相关时
    /// 保持上下文连续，不新建）；省略时新建。
    fn handle_dispatch_work(
        &self,
        task: &str,
        model: Option<&str>,
        cwd: Option<&str>,
        session_id: Option<&str>,
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
        // 复用已有会话（应用尽用）：校验会话存在，复用其 ID 保持上下文连续
        let reuse_session = session_id
            .filter(|id| !id.trim().is_empty())
            .map(str::trim)
            .map(str::to_string);
        if let Some(existing) = reuse_session.as_ref() {
            if is_central_session(existing) {
                return self.error_result("不能复用中枢对话自身作为工作会话".to_string());
            }
            if tiangong_core_manager::SessionMetadata::load_from_storage(
                self.sessions.storage_root(),
                existing,
            )
            .is_err()
            {
                return self.error_result(format!(
                    "复用的会话不存在：{existing}（可用 list_sessions 查询近期会话）"
                ));
            }
        }
        let reused = reuse_session.is_some();
        let session_id = reuse_session.unwrap_or_else(|| scru128::new().to_string());
        let record = WorkItemRecord {
            // work_id 用完整 scru128：与会话 ID 同风格，唯一性无妥协
            // （此前截短曾在同毫秒内撞车）。
            work_id: format!("work-{}", scru128::new()),
            reused_session: reused,
            session_id: session_id.clone(),
            task: task.to_string(),
            model: model
                .filter(|key| !key.trim().is_empty())
                .map(str::trim)
                .map(str::to_string),
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            finished_at: None,
        };
        let request = CentralDispatchRequest {
            work_id: record.work_id.clone(),
            session_id: session_id.clone(),
            task: record.task.clone(),
            model: record.model.clone(),
            cwd: cwd
                .filter(|path| !path.trim().is_empty())
                .map(str::trim)
                .map(str::to_string),
            reused_session: reused,
        };
        if let Err(error) = self
            .dispatch_tx
            .send(CentralPluginMessage::Dispatch(request))
        {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        self.upsert_memory_work_item(record.clone());
        upsert_work_item(self.sessions.storage_root(), &record);
        let model_note = record
            .model
            .as_deref()
            .unwrap_or("路由默认模型")
            .to_string();
        let session_note = if record.reused_session {
            format!("{session_id}（复用已有会话，上下文连续）")
        } else {
            format!("{session_id}（新会话；普通模式会话列表可查看）")
        };
        // 机制兜底 1：高相似度自动检出——无 session_id 且历史派发中有任务
        // 前缀高度一致（前 12 字符）的近期会话时，置顶强提示复用目标。
        let similarity_hint = {
            let items = self
                .work_items
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prefix: String = task.chars().take(12).collect();
            items
                .iter()
                .rev()
                .take(20)
                .find(|item| {
                    let item_prefix: String = item.task.chars().take(12).collect();
                    item_prefix == prefix
                })
                .map(|item| {
                    format!(
                        "\n\n⚠⚠ 检测到高度相似的历史派发：会话 {}（{}，任务「{}」）。\
                         本任务极可能是它的延续——若确认，应告知用户并向该会话重派\
                         （dispatch_work 带 session_id={}?模型参数可换，上下文保留）；\
                         仅当确属无关的新工作时才保留本次新建。",
                        item.session_id,
                        item.work_id,
                        item.task.chars().take(40).collect::<String>(),
                        item.session_id
                    )
                })
                .unwrap_or_default()
        };
        // 机制兜底 2：未指定复用但存在既往派发时，附提示供模型自查——
        // 若本任务是其中某项的延续，应告知用户并向该会话重派。
        let prior_note = if record.reused_session {
            String::new()
        } else {
            let items = self
                .work_items
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let recent: Vec<String> = items
                .iter()
                .filter(|item| item.work_id != record.work_id)
                .rev()
                .take(5)
                .map(|item| {
                    format!(
                        "  · {} → 会话 {}｜{}",
                        item.work_id,
                        item.session_id,
                        item.task.chars().take(30).collect::<String>()
                    )
                })
                .collect();
            if recent.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\n⚠ 本次未指定复用会话（新建）。既往派发：\n{}\n若本任务是其中某项的继续/追加/复查，应向用户说明并向对应会话重派（dispatch_work 带 session_id）；新会话无需处理。",
                    recent.join("\n")
                )
            }
        };
        let body = format!(
            "已派发后台工作：\n- work_id: {work_id}\n- 执行模型: {model_note}\n\
             - 工作会话: {session_note}\n- 任务: {task}\n\n\
             结果按时机自动回到本对话（失败立即；成功待全部结束后汇总一轮），\
             届时由你向用户转述。执行中如需查看可调 list_work_items。{prior_note}{similarity_hint}",
            work_id = record.work_id,
            model_note = model_note,
            session_note = session_note,
            task = record.task,
            prior_note = prior_note,
            similarity_hint = similarity_hint,
        );
        self.text_result("已派发", body)
    }

    /// 近期会话清单（派发复用的选择依据：应用尽用，§16 Worker 会话
    /// 可在普通模式查看，同样可被中枢继续使用）。
    fn handle_list_sessions(&self) -> Option<ToolResult> {
        let mut items = self.sessions.list_session_metadata();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let lines: Vec<String> = items
            .into_iter()
            .take(20)
            .map(|meta| {
                format!(
                    "- {}｜{}｜{} 条消息｜更新于 {}｜cwd {}",
                    meta.id, meta.title, meta.message_count, meta.updated_at, meta.cwd
                )
            })
            .collect();
        let body = if lines.is_empty() {
            "当前没有任何会话。".to_string()
        } else {
            format!(
                "近期会话（按更新时间倒序，前 20 条）：\n{}",
                lines.join("\n")
            )
        };
        self.text_result("近期会话", body)
    }

    /// 搜索历史会话消息：标题走元数据缓存，正文扫描近期会话（子串匹配，
    /// spawn_blocking 避免阻塞运行时）。返回命中片段供定位。
    fn handle_search_history(
        &self,
        query: &str,
        limit: usize,
        scope_all: bool,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        const RECENT_SESSIONS: usize = 200;
        const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
        const CONTEXT_CHARS: usize = 60;
        let query = query.trim().to_string();
        let limit = limit.clamp(1, 30);
        // &self 的使用全部在构造 future 之前完成（async 块不借用 self）
        let storage_root = self.sessions.storage_root().to_path_buf();
        let mut metas = self.sessions.list_session_metadata();
        metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let candidates: Vec<_> = if scope_all {
            metas
        } else {
            metas.into_iter().take(RECENT_SESSIONS).collect()
        };
        let empty_query = query.is_empty();
        let scope_note = if scope_all {
            "全部会话".to_string()
        } else {
            format!("最近 {RECENT_SESSIONS} 个会话")
        };
        let query_display = query.clone();
        Box::pin(async move {
            if empty_query {
                return None;
            }
            let query_for_task = query.clone();
            let hits = tokio::task::spawn_blocking(move || {
                let mut hits: Vec<String> = Vec::new();
                for meta in candidates {
                    if hits.len() >= limit {
                        break;
                    }
                    // 标题命中
                    if meta.title.contains(&query) {
                        hits.push(format!(
                            "- 会话 {}（{}）｜标题命中｜更新于 {}",
                            meta.id, meta.title, meta.updated_at
                        ));
                        continue;
                    }
                    let path = storage_root
                        .join("sessions")
                        .join(format!("{}.json", meta.id));
                    let Ok(file_meta) = std::fs::metadata(&path) else {
                        continue;
                    };
                    if file_meta.len() > MAX_FILE_BYTES {
                        continue; // 超大文件跳过正文扫描（防内存峰值）
                    }
                    let Ok(content) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                        continue;
                    };
                    let Some(messages) = value.get("messages").and_then(|m| m.as_array()) else {
                        continue;
                    };
                    for message in messages {
                        if hits.len() >= limit {
                            break;
                        }
                        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
                            continue;
                        };
                        for block in blocks {
                            let Some(text) = block.get("text").and_then(|t| t.as_str()) else {
                                continue;
                            };
                            if let Some(pos) = text.find(&query_for_task) {
                                let start = pos.saturating_sub(CONTEXT_CHARS);
                                let end =
                                    (pos + query_for_task.len() + CONTEXT_CHARS).min(text.len());
                                let snippet: String = text[start..end].replace('\n', " ");
                                let role =
                                    message.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                                let created = message
                                    .get("created_at")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("");
                                hits.push(format!(
                                    "- 会话 {}（{}）｜{role}｜{created}\n  …{snippet}…",
                                    meta.id, meta.title
                                ));
                                break;
                            }
                        }
                    }
                }
                hits
            })
            .await
            .unwrap_or_default();
            let body = if hits.is_empty() {
                format!("在{scope_note}中未找到「{query_display}」。")
            } else {
                format!(
                    "在{scope_note}中命中 {} 条（限 {limit}）：\n{}",
                    hits.len(),
                    hits.join("\n")
                )
            };
            // text_result 是 &self 方法——这里在 async 块中重建结果
            Some(ToolResult {
                ok: true,
                summary: "历史搜索".to_string(),
                stdout: body,
                stderr: String::new(),
                exit_code: 0,
                error_kind: None,
                retryable: None,
                execution: None,
            })
        })
    }

    /// 内存登记 upsert（与持久化同语义：同会话更新，不堆积）。
    fn upsert_memory_work_item(&self, record: WorkItemRecord) {
        let mut items = self
            .work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(idx) = items
            .iter()
            .position(|item| item.session_id == record.session_id)
        {
            items.remove(idx);
        }
        items.push(record);
    }

    /// 收集全部已到达的后台工作回报（取走语义：收集后清空暂存）。
    fn handle_collect_work_reports(&self) -> Option<ToolResult> {
        let reports = std::mem::take(
            &mut *self
                .work_reports
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if !reports.is_empty() {
            // 通知 app 同步状态登记（待汇报条目清零）并向前端发刷新事件
            let _ = self
                .dispatch_tx
                .send(CentralPluginMessage::ReportsCollected);
        }
        if reports.is_empty() {
            return self.text_result("后台工作回报", "当前没有待汇报的后台工作回报。".to_string());
        }
        let count = reports.len();
        let body = format!(
            "共 {count} 条后台工作回报（已全部收集，请向用户统一转述）：\n\n{}",
            reports.join("\n\n---\n\n")
        );
        self.text_result("后台工作回报", body)
    }

    /// 向已有 Worker 会话发送引导消息：调整方向、补充要求、追问细节。
    fn handle_send_to_worker(&self, session_id: &str, message: &str) -> Option<ToolResult> {
        let message = message.trim();
        if message.is_empty() {
            return self.error_result("message 不能为空".to_string());
        }
        let target = session_id.trim().to_string();
        if target.is_empty() {
            return self.error_result(
                "session_id 不能为空（list_work_items / list_sessions 查询）".to_string(),
            );
        }
        if is_central_session(&target) {
            return self.error_result("不能向中枢对话自身发送引导消息".to_string());
        }
        if tiangong_core_manager::SessionMetadata::load_from_storage(
            self.sessions.storage_root(),
            &target,
        )
        .is_err()
        {
            return self.error_result(format!(
                "会话不存在：{target}（可用 list_work_items / list_sessions 查询）"
            ));
        }
        let record = WorkItemRecord {
            work_id: format!("steer-{}", scru128::new()),
            reused_session: true,
            session_id: target.clone(),
            task: message.to_string(),
            model: None,
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            finished_at: None,
        };
        let request = CentralDispatchRequest {
            work_id: record.work_id.clone(),
            session_id: target,
            task: record.task.clone(),
            model: None,
            cwd: None,
            reused_session: true,
        };
        if let Err(error) = self.dispatch_tx.send(CentralPluginMessage::Steer(request)) {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        self.upsert_memory_work_item(record.clone());
        upsert_work_item(self.sessions.storage_root(), &record);
        let body = format!(
            "已发送引导消息：\n- 目标会话: {}\n- work_id: {}（终态同样回到本对话）\n- 内容: {}",
            record.session_id, record.work_id, record.task
        );
        self.text_result("引导已发送", body)
    }

    /// 派发登记表（中枢视角的派发事实）。
    fn handle_list_work_items(&self) -> Option<ToolResult> {
        let items = self
            .work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if items.is_empty() {
            return self.text_result(
                "后台工作",
                "当前没有派发记录（若应用刚重启，既往工作会话仍在——请用                  list_sessions 查询并按需复用）。".to_string(),
            );
        }
        let lines: Vec<String> = items
            .iter()
            .rev()
            .take(10)
            .map(|item| {
                let task_preview: String = item.task.chars().take(60).collect();
                format!(
                    "- {}｜模型 {}｜会话 {}｜派发于 {}\n  任务: {}",
                    item.work_id,
                    item.model.as_deref().unwrap_or("（默认）"),
                    item.session_id,
                    item.dispatched_at,
                    task_preview
                )
            })
            .collect();
        self.text_result(
            "后台工作",
            format!(
                "最近 {} 条（全部历史见 work-items.json）：\n{}",
                lines.len(),
                lines.join("\n")
            ),
        )
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
                        "session_id": {"type": "string", "description": "复用的已有会话 ID（list_sessions 查询）。任务与某会话相关（同工作区、同主题、上次未完成）时必须优先复用，保持上下文连续；仅无相关会话时省略新建"}
                    },
                    "required": ["task"]
                }),
            },
            ToolSpec {
                name: "search_history".to_string(),
                description: "搜索历史会话消息（跨会话）：标题走元数据缓存，正文扫描                    近期会话（子串匹配），返回命中片段。用户问「之前谁说过什么/某结论                    在哪个会话/以前怎么处理的」时使用；也可用于派发前确认是否已有                    相关工作。scope=all 扫全部（较慢），默认近期 200 个会话。"
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

impl ToolOverrideHandler for CentralPlugin {
    fn handle(
        &self,
        call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        // search_history 返回异步 future（spawn_blocking 扫描），单独走
        if call.name == "search_history" {
            let query = call
                .arguments
                .get("query")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let limit = call
                .arguments
                .get("limit")
                .and_then(|value| value.as_u64())
                .unwrap_or(10) as usize;
            let scope_all = call
                .arguments
                .get("scope")
                .and_then(|value| value.as_str())
                .is_some_and(|scope| scope == "all");
            return self.handle_search_history(query, limit, scope_all);
        }
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
            "list_sessions" => self.handle_list_sessions(),
            "send_to_worker" => {
                let session_id = call
                    .arguments
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let message = call
                    .arguments
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_send_to_worker(session_id, message)
            }
            "dispatch_work" => {
                let task = call
                    .arguments
                    .get("task")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let model = call.arguments.get("model").and_then(|value| value.as_str());
                let cwd = call.arguments.get("cwd").and_then(|value| value.as_str());
                let session_id = call
                    .arguments
                    .get("session_id")
                    .and_then(|value| value.as_str());
                self.handle_dispatch_work(task, model, cwd, session_id)
            }
            "collect_work_reports" => self.handle_collect_work_reports(),
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
        tokio::sync::mpsc::UnboundedReceiver<CentralPluginMessage>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let reports: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            CentralPlugin::new(workspace_dir, manager, tx, std::sync::Arc::clone(&reports)),
            rx,
            reports,
        )
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
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        assert_eq!(plugin.id(), "central");
        let sections = plugin.prompt_sections();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("天工中枢"));
        assert!(sections[0].contains("先调用工具了解情况再回答"));
        let specs = plugin.tool_specs();
        assert!(
            specs.iter().any(|spec| spec.name == "list_sessions"),
            "list_sessions 必须有声明（review o：有 handler 无 spec 则模型不可见）"
        );
        assert!(
            specs
                .iter()
                .find(|spec| spec.name == "dispatch_work")
                .is_some_and(|spec| spec
                    .input_schema
                    .get("properties")
                    .and_then(|props| props.get("session_id"))
                    .is_some()),
            "dispatch_work 声明必须包含 session_id（复用入口）"
        );
    }

    /// 工具声明与 handler 一致性：spec 集合 == 可处理集合（防再脱节）。
    #[test]
    fn central_tool_specs_match_handlers() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let spec_names: std::collections::HashSet<String> =
            plugin.tool_specs().into_iter().map(|s| s.name).collect();
        // handler 侧逐一探测（以返回结果非“不拦截”为准的成本高；这里以
        // 关键工具在双方都存在为断言，覆盖脱节的主要形态：spec 缺失）
        for name in [
            "get_context",
            "list_workspaces",
            "inspect_workspace",
            "list_models",
            "list_sessions",
            "dispatch_work",
            "send_to_worker",
            "search_history",
            "collect_work_reports",
            "list_work_items",
        ] {
            assert!(spec_names.contains(name), "工具 {name} 缺少声明");
            if name != "search_history" {
                // search_history 需要 tokio reactor（独立用例覆盖）
                let result = call_tool(&plugin, name, serde_json::json!({}));
                let _ = result;
            }
        }
    }

    /// 复用校验：中枢对话自身不可作为工作会话（review q）。
    #[test]
    fn dispatch_rejects_reusing_central_hub_itself() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "自查", "session_id": CENTRAL_SESSION_ID}),
        );
        assert!(!result.ok);
        assert!(result.stderr.contains("中枢对话自身"));
        assert!(dispatch_rx.try_recv().is_err(), "守卫拒绝不得派发");
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
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
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
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "审查仓库提交", "model": "k3", "cwd": "/tmp"}),
        );
        assert!(result.ok, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("已派发后台工作"));
        let message = dispatch_rx.try_recv().expect("派发请求应经通道发出");
        let request = match message {
            CentralPluginMessage::Dispatch(request) => request,
            other => panic!("应收到 Dispatch 消息，实际 {other:?}"),
        };
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
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
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
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
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

        let (plugin, _dispatch_rx, _reports) = plugin_with_channel(
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
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
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
        let (plugin, _dispatch_rx, _reports) =
            plugin_with_channel(root.path().display().to_string(), manager);
        let result = call_tool(&plugin, "list_workspaces", serde_json::json!({}));
        assert!(result.ok);
        // 默认工作区在列（去重后仅一次 dev/tiangong）
        assert!(result.stdout.contains("（默认）"));
        assert_eq!(result.stdout.matches("dev/tiangong").count(), 1);
        assert!(result.stdout.contains("git 分支 poc/central-mode"));
    }
    /// 应用尽用兜底：无 session_id 的后续派发，返回应附既往派发提示。
    #[test]
    fn dispatch_without_reuse_hints_prior_works() {
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
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        // 第一次派发（新建）：无既往，无提示
        let first = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "第一轮 review"}),
        );
        assert!(first.ok);
        assert!(!first.stdout.contains("既往派发"));
        // 第二次派发仍未指定 session_id：应提示既往派发供自查
        let second = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "继续 review 新增内容"}),
        );
        assert!(second.ok);
        assert!(second.stdout.contains("既往派发"));
        assert!(second.stdout.contains(
            "继续 review 新增内容"
                .chars()
                .take(30)
                .collect::<String>()
                .as_str()
        ));
        assert!(second.stdout.contains("向对应会话重派"));
    }

    /// 回报批量模式：暂存 → collect 取走 → 再 collect 为空。
    #[test]
    fn collect_work_reports_takes_all_then_empty() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        reports
            .lock()
            .unwrap()
            .push("[后台工作完成] work-a：结果一".to_string());
        reports
            .lock()
            .unwrap()
            .push("[后台工作完成] work-b：结果二".to_string());

        let first = call_tool(&plugin, "collect_work_reports", serde_json::json!({}));
        assert!(first.ok);
        assert!(first.stdout.contains("共 2 条"));
        assert!(first.stdout.contains("work-a"));
        assert!(first.stdout.contains("work-b"));

        let second = call_tool(&plugin, "collect_work_reports", serde_json::json!({}));
        assert!(second.ok);
        assert!(second.stdout.contains("没有待汇报"));
    }
    /// 引导消息：校验 + 登记 + 通道消息类型。
    #[test]
    fn send_to_worker_validates_and_sends_steer() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        // 目标不存在 → 拒绝
        let missing = call_tool(
            &plugin,
            "send_to_worker",
            serde_json::json!({"session_id": "nope", "message": "调整"}),
        );
        assert!(!missing.ok);
        assert!(missing.stderr.contains("会话不存在"));
        // 建一个会话文件后 → 通过并发送 Steer
        let target = root.path().join("sessions/worker-1.json");
        std::fs::write(
            &target,
            serde_json::json!({
                "id": "worker-1", "title": "w", "created_at": "2026-11-10 10:00:00",
                "updated_at": "2026-11-10 10:00:00", "trust_mode": "Supervised",
                "cwd": "/tmp", "messages": []
            })
            .to_string(),
        )
        .unwrap();
        let ok = call_tool(
            &plugin,
            "send_to_worker",
            serde_json::json!({"session_id": "worker-1", "message": "重点看事件流部分"}),
        );
        assert!(ok.ok, "stderr: {}", ok.stderr);
        match dispatch_rx.try_recv().expect("应有通道消息") {
            CentralPluginMessage::Steer(request) => {
                assert_eq!(request.session_id, "worker-1");
                assert_eq!(request.task, "重点看事件流部分");
                assert!(request.work_id.starts_with("steer-"));
            }
            other => panic!("应收到 Steer，实际 {other:?}"),
        }
        // 中枢自身 → 拒绝
        let self_send = call_tool(
            &plugin,
            "send_to_worker",
            serde_json::json!({"session_id": CENTRAL_SESSION_ID, "message": "x"}),
        );
        assert!(!self_send.ok);
    }
    /// 跨会话历史搜索：标题与正文命中、片段返回。
    #[tokio::test]
    async fn search_history_finds_title_and_body() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        // 两个会话：一个标题命中，一个正文命中
        std::fs::write(
            root.path().join("sessions/s-title.json"),
            serde_json::json!({
                "id": "s-title", "title": "中枢模式开发准备", "created_at": "2026-11-10 10:00:00",
                "updated_at": "2026-11-10 11:00:00", "trust_mode": "Supervised",
                "cwd": "/tmp", "messages": []
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.path().join("sessions/s-body.json"),
            serde_json::json!({
                "id": "s-body", "title": "其他", "created_at": "2026-11-10 10:00:00",
                "updated_at": "2026-11-10 12:00:00", "trust_mode": "Supervised",
                "cwd": "/tmp",
                "messages": [
                    {"id": "m1", "role": "assistant", "created_at": "2026-11-10 12:00:01",
                     "content": [{"type": "text", "text": "中枢模式的设计模式调整已经完成"}]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let call = ToolCall {
            id: "t1".to_string(),
            name: "search_history".to_string(),
            arguments: serde_json::json!({"query": "中枢模式"}),
        };
        let mut session = Session::new("测试");
        let result = ToolOverrideHandler::handle(&plugin, &call, &mut session, "test")
            .await
            .expect("应有结果");
        assert!(result.ok);
        assert!(
            result.stdout.contains("s-title"),
            "标题命中：{}",
            result.stdout
        );
        assert!(
            result.stdout.contains("设计模式调整"),
            "正文片段：{}",
            result.stdout
        );
        // 无命中
        let call2 = ToolCall {
            id: "t2".to_string(),
            name: "search_history".to_string(),
            arguments: serde_json::json!({"query": "绝不存在的关键词xyz"}),
        };
        let result2 = ToolOverrideHandler::handle(&plugin, &call2, &mut session, "test")
            .await
            .expect("应有结果");
        assert!(result2.ok);
        assert!(result2.stdout.contains("未找到"));
    }
    /// 派发登记会话维度 upsert：同会话再派发更新而非堆积；上限 20 条。
    #[test]
    fn work_items_upsert_by_session_and_cap_20() {
        let root = tempfile::tempdir().unwrap();
        // 预置 25 条不同会话的记录
        for i in 0..25 {
            let record = WorkItemRecord {
                work_id: format!("work-old-{i}"),
                reused_session: false,
                session_id: format!("sess-{i}"),
                task: format!("旧任务 {i}"),
                model: None,
                dispatched_at: format!("2026-11-10 1{:02}:00:00", i % 10),
                finished_at: Some("2026-11-10 12:00:00".to_string()),
            };
            upsert_work_item(root.path(), &record);
        }
        // 同一会话 sess-24 再派发 → 更新（不新增）
        let again = WorkItemRecord {
            work_id: "work-new-1".to_string(),
            reused_session: true,
            session_id: "sess-24".to_string(),
            task: "继续上次的任务".to_string(),
            model: Some("k3".to_string()),
            dispatched_at: "2026-11-10 15:00:00".to_string(),
            finished_at: None,
        };
        upsert_work_item(root.path(), &again);
        let items = load_work_items(root.path());
        assert_eq!(items.len(), 20, "上限 20：{}", items.len());
        // sess-24 只有一条且是更新后的
        let hits: Vec<_> = items
            .iter()
            .filter(|item| item.session_id == "sess-24")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].work_id, "work-new-1");
        assert_eq!(hits[0].task, "继续上次的任务");
        assert!(hits[0].reused_session);
        // 最旧的 sess-0 已被裁剪
        assert!(!items.iter().any(|item| item.session_id == "sess-0"));
    }
}
