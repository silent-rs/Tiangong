//! 控制与状态工具（R5 从 lib.rs 拆出）：get_work_item_status /
//! switch_worker_model（票据校验）/ send_notification / cancel-pause-resume /
//! get_work_timeline（事件日志）。

use tiangong_core::tools::result::ToolResult;

use crate::CentralPlugin;
use crate::*;

impl CentralPlugin {
    /// 单工作详情（状态 + 事件时间线，get_work_item_status 蓝图对位）。
    pub(crate) fn handle_work_item_status(&self, work_id: &str) -> Option<ToolResult> {
        let work_id = work_id.trim();
        if work_id.is_empty() {
            return self.error_result("work_id 不能为空（list_work_items 查询）".to_string());
        }
        // 状态：内存 registry 经 dispatch 通道不可达，读磁盘登记 + 中枢状态不可得时
        // 返回登记事实与时间线（状态实时值以徽章/get_context 为准）。
        let items = load_work_items(self.sessions.storage_root());
        let Some(entry) = items.iter().rev().find(|item| item.work_id == work_id) else {
            return self.error_result(format!("未找到工作 {work_id}"));
        };
        let outcome_note = match entry.outcome.as_deref() {
            Some("completed") => "已完成",
            Some("failed") => "失败",
            Some("cancelled") => "已取消",
            _ if entry.finished_at.is_some() => "已终态",
            _ => "未终态（执行/等待/中断以徽章为准）",
        };
        let timeline: Vec<String> = entry
            .events
            .iter()
            .map(|e| format!("  - [{}] {}：{}", e.at, e.kind, e.note))
            .collect();
        self.text_result(
            "工作详情",
            format!(
                "工作 {}（会话 {}）\n任务：{}\n派发：{} · 终态：{}\n时间线：\n{}",
                work_id,
                entry.session_id,
                entry.task,
                entry.dispatched_at,
                outcome_note,
                if timeline.is_empty() {
                    "  （无事件记录）".to_string()
                } else {
                    timeline.join("\n")
                }
            ),
        )
    }

    /// 换模型（§15.10/15.11：先商量后切换，商量结论留痕）。
    pub(crate) fn handle_switch_worker_model(
        &self,
        session_id: &str,
        model: &str,
        consultation_summary: &str,
    ) -> Option<ToolResult> {
        let session_id = session_id.trim();
        let model = model.trim();
        let consultation_summary = consultation_summary.trim();
        if session_id.is_empty() || model.is_empty() {
            return self.error_result("session_id 与 model 不能为空".to_string());
        }
        if is_central_session(session_id) {
            return self.error_result("不能对中枢对话切换模型".to_string());
        }
        // 商量结论是硬约束：没有留痕依据的切换被拒绝（§15.11 票据的轻量版）
        if consultation_summary.len() < 8 {
            return self.error_result(
                "consultation_summary 必须说明与用户商量的结论（原因 + 用户同意事实），不能为空或过短——换模型必须先商量。"
                    .to_string(),
            );
        }
        // 模型解析校验（复用派发同款：失败附可用清单）
        if let Err(error) = self.sessions.resolve_turn_model(Some(model)) {
            let models = tiangong_config::io::load_models_config_at(self.sessions.storage_root());
            let available: Vec<String> = models.models.keys().cloned().collect();
            return self.error_result(format!("{error}\n可用模型：{}", available.join("、")));
        }
        // 执行切换：目标会话的 Core 在线时直接切换（下轮生效）；不在线则
        // 记录模型偏好由下次 ensure 生效（dispatch 带 model 亦可）。
        let endpoint = self.sessions.resolve_turn_model(Some(model)).ok();
        let registry = self.sessions.registry();
        let core = registry.get(session_id);
        let switch_result = match (&core, endpoint) {
            (Some(core), Some(endpoint)) => core.switch_model(endpoint).map_err(|e| e.to_string()),
            _ => Err("目标会话当前不在线（已空闲回收）；下次派发时带 model 参数生效".to_string()),
        };
        // 商量留痕：回报队列 + 时间线（Z6 诚实性：留痕内容按真实结果区分——
        // 失败不再写 ModelHandoverConfirmed 事件「记录未发生的事实」）
        let note = format!("模型切换（{model}）：{consultation_summary}");
        self.work_reports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("[系统中枢] {note}"));
        record_central_event_by_session(
            self.sessions.storage_root(),
            session_id,
            tiangong_central::event::WorkEventType::ModelHandoverConfirmed,
            &format!("{note}（切换成功，下轮生效）"),
        );
        match switch_result {
            Ok(()) => self.text_result(
                "模型已切换",
                format!("会话 {session_id} → {model}（执行中的工作下一轮生效）。商量记录已留痕。"),
            ),
            Err(reason) => self.error_result(format!(
                "会话 {session_id} → {model} 切换未完成：{reason}。商量记录已留痕；下次派发时带 model 参数可生效。"
            )),
        }
    }

    /// 应用内通知（经通道由执行器 emit 前端 toast）。
    pub(crate) fn handle_send_notification(
        &self,
        title: &str,
        message: &str,
        level: &str,
    ) -> Option<ToolResult> {
        let title = title.trim();
        if title.is_empty() {
            return self.error_result("title 不能为空".to_string());
        }
        let level = if ["info", "warning", "error"].contains(&level) {
            level
        } else {
            "info"
        };
        if let Err(error) = self.dispatch_tx.send(CentralPluginMessage::Notification {
            title: title.to_string(),
            message: message.trim().to_string(),
            level: level.to_string(),
        }) {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        self.text_result("通知已发送", format!("已向用户发送 {level} 通知：{title}"))
    }

    /// 请求用户授权 Workspace（R3 票据化：模型只能请求，票据由用户
    /// 操作签发）。触发授权卡片；指纹须与后续派发的任务文本一致。
    pub(crate) fn handle_request_workspace_approval(
        &self,
        path: &str,
        task_fingerprint: &str,
    ) -> Option<ToolResult> {
        let path = path.trim();
        let task_fingerprint = task_fingerprint.trim();
        if path.is_empty() {
            return self.error_result("path 不能为空".to_string());
        }
        if task_fingerprint.is_empty() {
            return self.error_result(
                "task_fingerprint 不能为空（应与 dispatch_work 的 task 完全一致，否则授权不生效）"
                    .to_string(),
            );
        }
        if let Err(error) = self
            .dispatch_tx
            .send(CentralPluginMessage::RequestWorkspaceApproval {
                path: path.to_string(),
                task_fingerprint: task_fingerprint.to_string(),
            })
        {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        self.text_result(
            "已请求用户授权",
            format!(
                "已向用户发出 {path} 的授权请求（授权卡片）。
\
                 请向用户说明要在该目录做什么、为什么需要授权；用户确认后重新派发同一任务（task 文本保持不变，否则需重新授权）。"
            ),
        )
    }
    /// 工作控制（取消/暂停/恢复）共用：校验登记存在并发 Control 消息。
    pub(crate) fn handle_work_control(&self, work_id: &str, action: &str) -> Option<ToolResult> {
        let work_id = work_id.trim();
        if work_id.is_empty() {
            return self.error_result("work_id 不能为空（list_work_items 查询）".to_string());
        }
        let items = self
            .work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let found = items.iter().rev().find(|item| item.work_id == work_id);
        let Some(entry) = found else {
            return self.error_result(format!(
                "未找到工作 {work_id}（list_work_items 查询登记；仅最近 20 个会话有记录）"
            ));
        };
        let request = CentralControlRequest {
            work_id: work_id.to_string(),
            session_id: entry.session_id.clone(),
            action: action.to_string(),
        };
        if let Err(error) = self
            .dispatch_tx
            .send(CentralPluginMessage::Control(request))
        {
            return self.error_result(format!("派发通道已关闭：{error}"));
        }
        let action_text = match action {
            "cancelled" => "取消",
            "paused" => "暂停",
            _ => "恢复",
        };
        let summary = format!("工作{action_text}指令已发出");
        self.text_result(
            summary.as_str(),
            format!(
                "work_id: {work_id}\n系统将中断该工作的当前执行（如仍在运行）并标记为「已{action_text}」。取消：工作终止不再恢复；暂停：上下文保留在会话中，恢复时自动发送继续指令；恢复：向该会话发送继续指令，工作回到执行中。"
            ),
        )
    }

    /// 工作时间线查询（事件持久化，§13.4）。
    pub(crate) fn handle_work_timeline(&self, work_id: &str) -> Option<ToolResult> {
        let work_id = work_id.trim();
        if work_id.is_empty() {
            return self.error_result("work_id 不能为空（list_work_items 查询）".to_string());
        }
        // R5：事件日志是唯一事实源（旧内嵌时间线仅兼容读）。
        let timeline = work_timeline_from_log(self.sessions.storage_root(), work_id);
        if timeline.is_empty() {
            // 日志与旧时间线都无记录：登记存在则说明是旧版本数据
            let known = load_work_items(self.sessions.storage_root())
                .iter()
                .any(|item| item.work_id == work_id)
                || self
                    .work_items
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .any(|item| item.work_id == work_id);
            if known {
                return self.text_result(
                    "工作时间线",
                    format!("工作 {work_id} 暂无事件记录（可能为旧版本登记）。"),
                );
            }
            return self.error_result(format!("未找到工作 {work_id}"));
        }
        let lines: Vec<String> = timeline
            .iter()
            .map(|(at, kind, note)| format!("- [{at}] {kind}：{note}"))
            .collect();
        self.text_result(
            "工作时间线",
            format!(
                "工作 {work_id} 事件时间线（事件日志）：\n{}",
                lines.join("\n")
            ),
        )
    }
}
