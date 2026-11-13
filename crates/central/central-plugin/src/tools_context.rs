//! 只读上下文工具（R5 从 lib.rs 拆出）：get_context / list_workspaces /
//! inspect_workspace / list_models / list_sessions / search_history——
//! 「先了解情况再回答」的数据面。

use serde_json;
use tiangong_core::tools::result::ToolResult;

use crate::CentralPlugin;
use crate::*;

impl CentralPlugin {
    pub(crate) fn handle_get_context(&self) -> Option<ToolResult> {
        let workspace_count = self.known_workspace_paths().len();
        let pending_reports = self
            .work_reports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let interrupted_reports = interrupted_work_items(self.sessions.storage_root()).len();
        let goals_summary: String = {
            let goals = load_goals(self.sessions.storage_root());
            let active: Vec<String> = goals
                .iter()
                .filter(|g| g.status == "active")
                .map(|g| g.title.clone())
                .collect();
            if active.is_empty() {
                "无（用户表达目标时请用 create_goal 登记）".to_string()
            } else {
                active.join("、")
            }
        };
        let body = format!(
            "天工版本: {version}\n\
             中枢模式: POC 阶段 1（单一中枢对话 + 指定模型派发后台工作已接入）\n\
             身份边界: 中枢是控制面，不直接执行工作型任务（§5.2）\n\
             可用工具: get_context / list_workspaces / inspect_workspace / list_models / list_sessions / dispatch_work / send_to_worker / collect_work_reports / list_work_items / search_history（查询类只读；dispatch_work 派发、支持 session_id 复用；send_to_worker 发引导（等待用户决策的工作收到答复后自动恢复执行）；cancel_work/pause_work/resume_work 控制；get_work_timeline 查工作时间线；create_goal/update_goal_status 管理用户目标（用户表达目标时登记，达成/放弃时更新）；create_report/list_reports 结构化报告存档与查询；get_work_item_status 单工作详情；switch_worker_model 换工作会话模型（必须先与用户商量并携带结论）；send_notification 应用内通知（重要事件，勿滥用）；dispatch_work 支持 after_work_id 排队依赖；search_history 跨会话搜消息）\n\
             后台工作派发: 已接入（dispatch_work 指定模型派发后台会话执行，完成进入待汇报队列，随用户下一条消息一并进入对话，或由你调 collect_work_reports 主动收集）\n\
             默认工作区: {workspace}\n\
             已知工作区: {count} 个（list_workspaces 查看路径与 git 分支）\n\
             待汇报的后台工作回报: {pending} 条（可调 collect_work_reports 收集后转述）\n\
             中枢工具数: {tool_count}\n\
             因应用重启中断的任务: {interrupted} 个（徽章可见；结果未产出，可重派复用原会话）\n             当前用户目标: {goals}\n\
             当前时间: {now}",
            version = env!("CARGO_PKG_VERSION"),
            workspace = self.workspace_dir,
            count = workspace_count,
            pending = pending_reports,
            interrupted = interrupted_reports,
            goals = goals_summary,
            tool_count = self.tool_specs().len(),
            now = chrono::Local::now().format("%Y-%m-%d %H:%M"),
        );
        self.text_result("中枢上下文", body)
    }

    /// 聚合本机已知工作区路径：全局默认 + 各会话工作目录（元数据 cwd 去重）。
    pub(crate) fn known_workspace_paths(&self) -> Vec<String> {
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

    pub(crate) fn handle_list_workspaces(&self) -> Option<ToolResult> {
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
    pub(crate) fn handle_inspect_workspace(&self, path: &str) -> Option<ToolResult> {
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
    pub(crate) fn handle_list_models(&self) -> Option<ToolResult> {
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

    /// 近期会话清单（派发复用的选择依据：应用尽用，§16 Worker 会话
    /// 可在普通模式查看，同样可被中枢继续使用）。
    pub(crate) fn handle_list_sessions(&self) -> Option<ToolResult> {
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
    pub(crate) fn handle_search_history(
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
                                // UTF-8 安全边界（review N7）：字节偏移回退/前进
                                // 到字符边界，防止切片落在多字节字符中间 panic
                                let mut start = pos.saturating_sub(CONTEXT_CHARS);
                                while start < text.len() && !text.is_char_boundary(start) {
                                    start += 1;
                                }
                                let mut end =
                                    (pos + query_for_task.len() + CONTEXT_CHARS).min(text.len());
                                while end < text.len() && !text.is_char_boundary(end) {
                                    end += 1;
                                }
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
}
