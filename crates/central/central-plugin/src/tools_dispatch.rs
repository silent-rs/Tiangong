//! 派发与回报工具（R5 从 lib.rs 拆出）：dispatch_work（前置校验与派发）、
//! send_to_worker 引导、collect_work_reports、list_work_items 登记表。

use tiangong_core::tools::result::ToolResult;

use crate::CentralPlugin;
use crate::*;

impl CentralPlugin {
    /// 派发后台工作：校验模型与复用会话 → 登记 → 经通道交给 app 执行器。
    ///
    /// `session_id` 提供时复用该已有会话（应用尽用：任务与既有会话相关时
    /// 保持上下文连续，不新建）；省略时新建。
    pub(crate) fn handle_dispatch_work(
        &self,
        task: &str,
        model: Option<&str>,
        cwd: Option<&str>,
        session_id: Option<&str>,
        after_work_id: Option<&str>,
        parallel: bool,
    ) -> Option<ToolResult> {
        let task = task.trim();
        if task.is_empty() {
            return self.error_result("task 不能为空".to_string());
        }
        // Workspace 授权（§资源边界，R3 票据化）：向非默认工作区派发前，
        // 该工作区必须已获用户授权——授权是宿主事实（用户真实操作签发
        // 的一次性票据，或历史已确认工作区），助手不能自证同意
        // （confirm_workspace 已移除：模型可自调的登记通道不可信）。
        // 默认工作区及其子树按前缀天然放行（workspace_confirmed 内判定）。
        let unconfirmed = |dir: &str| -> Option<String> {
            let dir = dir.trim();
            if dir.is_empty() {
                return None;
            }
            if workspace_confirmed(self.sessions.storage_root(), &self.workspace_dir, dir) {
                return None;
            }
            Some(dir.to_string())
        };
        if let Some(cwd) = cwd.and_then(unconfirmed) {
            return self.error_result(format!(
                "工作区 {cwd} 尚未经用户授权（派发未执行）。请调 request_workspace_approval(path=\"{cwd}\", task_fingerprint=\"{task}\") 向用户发起授权卡片，并说明任务与影响；用户确认后重派同一任务。不要更换目录规避，也不要在授权前重复派发。"
            ));
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
            // 复用路径的 Workspace 确认（review P1 补齐）：复用会话沿用其
            // 自身 cwd 派发——该目录同样必须已确认，否则「被拒新目录派发
            // →复用旧会话」成为绕过通道（强制复用把模型恰好赶向这条路）。
            let session_cwd = tiangong_core_manager::SessionMetadata::load_from_storage(
                self.sessions.storage_root(),
                existing,
            )
            .ok()
            .map(|meta| meta.cwd)
            .unwrap_or_default();
            if let Some(dir) = unconfirmed(&session_cwd) {
                return self.error_result(format!(
                    "复用会话 {existing} 的工作目录 {dir} 尚未经用户授权（派发未执行）。请调 request_workspace_approval(path=\"{dir}\", task_fingerprint=\"{task}\") 向用户发起授权卡片；用户确认后重派同一任务。"
                ));
            }
        }
        let reused = reuse_session.is_some();
        let session_id = reuse_session.unwrap_or_else(|| scru128::new().to_string());
        // 机制兜底 1：高相似度自动检出（review N2：在登记 upsert **之前**计算并
        // 过滤自身，否则每次派发必命中自己，提示「向刚建的会话重派」）。
        let pending_work_id = format!("work-{}", scru128::new());
        // 强制复用（用户实测：提示式兜底被模型忽略，仍新建会话）：高相似
        // （任务前 12 字符一致）且未指定 session_id 且任务未标【全新】时
        // 直接拒绝，要求带 session_id 重发——延续性工作必须复用。
        let explicitly_new = task.contains("【全新】");
        if !reused && !explicitly_new {
            let items = self
                .work_items
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prefix: String = task.chars().take(12).collect();
            let similar = items
                .iter()
                .filter(|item| {
                    let item_prefix: String = item.task.chars().take(12).collect();
                    item_prefix == prefix
                })
                .rev()
                .take(20)
                .find(|item| item.work_id != pending_work_id);
            if let Some(item) = similar {
                return self.error_result(format!(
                    "检测到高度相似的历史派发：会话 {}（{}，任务「{}」）。\
                     本任务极可能是它的延续——请带 session_id={} 重新派发（上下文连续，\
                     模型参数可换）；仅当确属无关的全新工作时，在任务描述前加【全新】标记后重发。",
                    item.session_id,
                    item.work_id,
                    item.task.chars().take(40).collect::<String>(),
                    item.session_id
                ));
            }
        }
        let similarity_hint = String::new(); // 强制拒绝已替代提示式兜底
                                             // R0 契约（§5.4）：每次派发生成唯一执行归属，随请求、登记与
                                             // 事件时间线一起流动——日志与持久化记录据此唯一定位。
        let execution_id = tiangong_central::new_execution_id();
        let record = WorkItemRecord {
            work_id: pending_work_id,
            reused_session: reused,
            session_id: session_id.clone(),
            task: task.to_string(),
            model: model
                .filter(|key| !key.trim().is_empty())
                .map(str::trim)
                .map(str::to_string),
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            finished_at: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: cwd
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_string),
            outcome: None,
            execution_id: Some(execution_id.clone()),
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
            after_work_id: after_work_id.map(str::to_string),
            parallel,
            reused_session: reused,
            execution_id,
        };
        if let Err(error) = self
            .dispatch_tx
            .send(CentralPluginMessage::Dispatch(request))
        {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        // 机制兜底 2：未指定复用但存在其他既往派发时，附提示供模型自查
        // （排除本次 upsert 后的自己，review N2）。
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
        self.upsert_memory_work_item(record.clone());
        upsert_work_item(self.sessions.storage_root(), &record);
        // 复用时 record.model 是「被忽略的请求模型」（M5 提示用）——执行模型
        // 行必须报真实生效模型，避免与「已忽略请求的 X」同屏矛盾（review S5）
        let model_note = if record.reused_session {
            "会话既有模型".to_string()
        } else {
            record
                .model
                .as_deref()
                .unwrap_or("路由默认模型")
                .to_string()
        };
        let session_note = if record.reused_session {
            // 复用实际沿用会话既有模型（review M5：明确告知忽略的请求模型）
            match record.model.as_deref() {
                Some(requested) => format!(
                    "{session_id}（复用已有会话，沿用其既有模型；已忽略请求的 {requested}）"
                ),
                None => format!("{session_id}（复用已有会话，沿用其既有模型）"),
            }
        } else {
            format!("{session_id}（新会话；普通模式会话列表可查看）")
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

    /// 内存登记 upsert（与持久化同语义：同会话更新，不堆积）。
    pub(crate) fn upsert_memory_work_item(&self, record: WorkItemRecord) {
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
    pub(crate) fn handle_collect_work_reports(&self) -> Option<ToolResult> {
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
    pub(crate) fn handle_send_to_worker(
        &self,
        session_id: &str,
        message: &str,
    ) -> Option<ToolResult> {
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
        // R0 契约（§5.4）：引导同样是一次执行，生成独立归属。
        let execution_id = tiangong_central::new_execution_id();
        let record = WorkItemRecord {
            work_id: format!("steer-{}", scru128::new()),
            reused_session: true,
            session_id: target.clone(),
            task: message.to_string(),
            model: None,
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            finished_at: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            outcome: None,
            execution_id: Some(execution_id.clone()),
        };
        let request = CentralDispatchRequest {
            work_id: record.work_id.clone(),
            session_id: target,
            task: record.task.clone(),
            model: None,
            cwd: None,
            after_work_id: None,
            parallel: false,
            reused_session: true,
            execution_id,
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
    pub(crate) fn handle_list_work_items(&self) -> Option<ToolResult> {
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
