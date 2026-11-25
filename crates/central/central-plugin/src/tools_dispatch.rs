//! 派发与回报工具（R5 从 lib.rs 拆出）：dispatch_work（前置校验与派发）、
//! 引导（send_to_worker 已并入 dispatch_work 复用路径）与登记表。

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
        // Workspace 授权不在插件侧预检（2026-11-25 修复）：授权事实由宿主
        // 独占——用户点击「授权」签发的一次性票据只在宿主内存，插件仅看得到
        // 落盘的长期授权；此前按落盘授权预检，用户点「授权」（仅此一次）后
        // 重派仍被插件短路，票据永远消费不到。宿主 plan_central_dispatch 对
        // 请求目录与复用会话目录统一校验，拒绝时自动发出授权卡片并说明。
        // 插件只做模型 / 会话校验与登记。
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
                    "复用的会话不存在：{existing}（可用 inspect_context focus=sessions 查询近期会话）"
                ));
            }
            // 复用会话沿用其自身 cwd 派发——目录授权同样由宿主统一校验
            // （plan_central_dispatch 对最终 cwd 校验，含一次性票据），插件不预检。
        }
        let reused = reuse_session.is_some();
        let session_id = reuse_session.unwrap_or_else(|| scru128::new().to_string());
        // 机制兜底 1：高相似度自动检出（review N2：在登记 upsert **之前**计算并
        // 过滤自身，否则每次派发必命中自己，提示「向刚建的会话重派」）。
        // CC2 插件侧归属（二十一轮 review）：复用会话的派发是既有工作的
        // 延续（引导并入语义，与宿主 steer_merged_into_running / 重启投影
        // owner 优先一致）——沿用该会话最近登记的 work_id，不再生成新 id
        // 覆盖（此前新 id 覆盖导致 work status / control(owner) 报
        // 「未找到工作」、work list 任务文本被引导文本替换）。
        // S1（R28-7，探针 PR-04）：仅**未终态**（outcome/finished_at 为空）
        // 的在途工作沿用 work_id；上一工作已终态的复用是新工作——分配
        // 新 id。否则投影对同 id 的 Completed/Cancelled 绝对吸收会吞掉
        // 新 Started（重启后新执行显示旧终态）。
        let pending_work_id = if reused {
            load_work_items(self.sessions.storage_root())
                .into_iter()
                .rev()
                .find(|item| item.session_id == session_id)
                .map(|item| {
                    let finished = item.outcome.is_some() || item.finished_at.is_some();
                    if finished {
                        format!("work-{}", scru128::new())
                    } else {
                        item.work_id
                    }
                })
                .unwrap_or_else(|| format!("work-{}", scru128::new()))
        } else {
            format!("work-{}", scru128::new())
        };
        // 强制复用（用户实测：提示式兜底被模型忽略，仍新建会话）：高相似
        // （任务前 12 字符一致）且未指定 session_id 且任务未标【全新】时
        // 直接拒绝，要求带 session_id 重发——延续性工作必须复用。
        let explicitly_new = task.contains("【全新】");
        if !reused && !explicitly_new {
            let storage_root = self.sessions.storage_root();
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
                // 派发未被执行（宿主拒绝 / 投递失败）的登记不会产生会话文件：
                // 没有会话就无从复用，不得强制延续——否则授权后的重派会被
                // 「带 session_id 重发」与「复用的会话不存在」来回踢
                // （2026-11-25：插件不再预检授权后，宿主拒绝是常见路径）
                .filter(|item| {
                    tiangong_core_manager::SessionMetadata::load_from_storage(
                        storage_root,
                        &item.session_id,
                    )
                    .is_ok()
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
        let registry_write_failed =
            upsert_work_item(self.sessions.storage_root(), &record).is_err();
        if registry_write_failed {
            tracing::warn!("派发登记落盘失败（事件日志为准，会话复用记忆可能缺失）");
        }
        // model 是统一的换模型通道（二十轮合并 + 用户裁决）：复用会话
        // 同样生效——空闲则本轮切换，执行中则引导并入当前 turn、本轮
        // 沿用当前模型（deliver 层忙时跳过模型编排）。
        let model_note = match (record.reused_session, record.model.as_deref()) {
            (true, Some(requested)) => format!("切换为 {requested}"),
            (true, None) => "会话既有模型".to_string(),
            (false, model) => model.unwrap_or("路由默认模型").to_string(),
        };
        let session_note = match (record.reused_session, record.model.as_deref()) {
            (true, Some(requested)) => format!(
                "{session_id}（复用已有会话，切换为 {requested}：空闲本轮生效；                 若正在执行则本轮沿用当前模型）"
            ),
            (true, None) => format!("{session_id}（复用已有会话，沿用其既有模型）"),
            (false, _) => format!("{session_id}（新会话；普通模式会话列表可查看）"),
        };
        let body = format!(
            "已受理派发（排队/拒绝由宿主决定，状态经 work 可查）：\n- work_id: {work_id}\n- 执行模型: {model_note}\n\
             - 工作会话: {session_note}\n- 任务: {task}\n\n\
             派发请求已提交宿主：目标在途时自动排队（依赖终态后投递）、目录未授权会被拒——\
             两者都在本对话可见，被拒后需向用户说明而不是当作工作失败。\
             结果按时机自动回到本对话（失败立即；成功待全部结束后汇总一轮），\
             届时由你向用户转述。执行中如需查看可调 work（action=list）。{prior_note}{registry_note}",
            work_id = record.work_id,
            model_note = model_note,
            session_note = session_note,
            task = record.task,
            prior_note = prior_note,
            // 十九轮 P2-5：登记落盘失败显式提示（事件日志为准）
            registry_note = if registry_write_failed {
                "\n\n⚠ 派发登记落盘失败（事件日志为准；会话复用记忆可能缺失，重启后以事件重建。）"
            } else {
                ""
            }
        );
        self.text_result("已派发", body)
    }

    /// 内存登记 upsert（与持久化同语义：同会话更新，不堆积）。
    pub(crate) fn upsert_memory_work_item(&self, mut record: WorkItemRecord) {
        let mut items = self
            .work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(idx) = items
            .iter()
            .position(|item| item.session_id == record.session_id)
        {
            // R29 P2：与磁盘 upsert 同语义；同一未终态工作保留 owner
            // 原目标，list 不能展示引导/答复。新 work_id 使用新任务文本。
            let existing = &items[idx];
            if existing.work_id == record.work_id
                && existing.outcome.is_none()
                && existing.finished_at.is_none()
            {
                record.task = existing.task.clone();
            }
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
    pub(crate) fn handle_list_work_items(&self) -> Option<ToolResult> {
        let items = self
            .work_items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if items.is_empty() {
            return self.text_result(
                "后台工作",
                "当前没有派发记录（若应用刚重启，既往工作会话仍在——请用 inspect_context（focus=sessions）查询并按需复用）。".to_string(),
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
