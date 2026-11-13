//! 后台工作终态与回报（R1 从 app.rs 拆出）：on_central_work_finished 终态处理
//! （取消/暂停守卫、等待用户分流、成败分态）、排队派发 flush、状态条查询、
//! 回报暂存队列（失败必达 / 成功汇总的时机判定）。

use central_state::{should_deliver_now, transition_on_finished, WorkOutcome};
use tiangong_core::agent_input::AgentInputKind;

use crate::app::TiangongApp;

use super::dispatch::handle_central_dispatch;
use super::events::emit_central_work_updated_state;
use super::state_of;

impl TiangongApp {
    /// 向中枢对话注入一条后台工作回报消息（完成/失败），触发中枢自动
    /// 转述（事件驱动汇报的最小闭环）。中枢空闲时开启新 turn；执行中
    /// 作为引导消息排队。
    /// 后台工作终态到达：登记移除 + 回报暂存 + **汇报时机判定**。
    ///
    /// 时机规则（设计稿 §9「关键节点必达 / 适时沉默」，意义与轻重的
    /// 区分由中枢模型在汇报轮内判断）：
    /// - **失败必达**：立即注入全部暂存并触发汇报轮（失败常需用户知晓）；
    /// - **成功且仍有任务在执行**：仅暂存——与后续任务汇总；
    /// - **成功且是最后一个（无在执行任务）**：注入全部暂存，触发一次
    ///   汇总轮，由中枢区分轻重统一回复。
    pub(crate) fn on_central_work_finished(
        &self,
        request: &crate::central::CentralDispatchRequest,
        done: bool,
        notice: &str,
    ) {
        // 阻塞识别（用户决策协议）：回报含【需要用户决策】→ waiting_user
        // （未终态），立即注入中枢（阻塞必达），中枢在对话中问用户；
        let outcome = central_state::classify_outcome(notice);
        // R0 观测：终态回调按 execution_id 唯一定位（快速完成 / 迟到回调
        // 均可在日志中区分归属），成败与结局一并记录。
        tracing::info!(
            execution_id = %request.execution_id,
            work_id = %request.work_id,
            session_id = %request.session_id,
            done,
            outcome = outcome.as_str(),
            "后台工作终态回调"
        );
        self.central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request.work_id);
        // 控制（取消/暂停）守卫：用户已显式取消/暂停的工作，随后的流终态
        // （Cancel → Done）不得把状态改回 report_queued/failed，也不注入
        // 「完成」回报（设计稿 §9.4 适时沉默——控制结果徽章可见即可）。
        let current_state = {
            let registry = self
                .central_work_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry
                .iter()
                .rev()
                .find(|entry| entry.session_id == request.session_id)
                .map(|entry| entry.state)
        };
        // 转移决策统一走 central-state（取消/暂停守卫 + 等待用户分流 + 成败
        // 分态——行为锁定于 crate 测试，宿主不再自持规则）。
        let next_state = transition_on_finished(current_state, outcome, done);
        if current_state.is_some_and(|s| s == central_state::WorkState::Cancelled)
            || current_state.is_some_and(|s| s == central_state::WorkState::Paused)
        {
            emit_central_work_updated_state(self);
            return;
        }
        self.central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(notice.to_string());
        // 状态登记：等待用户 → waiting_user（未终态）；失败 → failed
        // （与完成严格分态）；成功 → report_queued。
        {
            let mut registry = self
                .central_work_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // 查找与落盘同款兜底：优先 work_id，找不到按 session_id——
            // 会话聚合 upsert 后，同会话先派发 work 的条目可能已被覆盖，
            // 终态仍应更新到该会话现有条目（状态不得因聚合丢失）。
            let position = registry
                .iter()
                .position(|entry| entry.work_id == request.work_id)
                .or_else(|| {
                    registry
                        .iter()
                        // W4：session 兜底不得命中 Queued——排队条目未被
                        // 投递过，写入终态会让重启恢复错乱（Queued 非终态，
                        // R2 守卫拦不住）
                        .position(|entry| {
                            entry.session_id == request.session_id
                                && entry.state != central_state::WorkState::Queued
                        })
                });
            if let Some(position) = position {
                registry[position].state = next_state;
            }
        }
        if matches!(outcome, WorkOutcome::WaitingUser) {
            // 阻塞必达 + 不落终态（等待用户答复后经引导恢复执行）
            // R5 单写：等待用户事实入事件日志（唯一事实源）。
            super::central_events::record_for(
                self,
                &request.work_id,
                &request.execution_id,
                &request.session_id,
                tiangong_central::event::WorkEventType::WorkItemWaitingUser,
                "提出需要用户决策的问题，工作中挂起",
            );
            self.deliver_pending_work_reports();
            emit_central_work_updated_state(self);
            return;
        }
        // R5 单写：终态事实入事件日志（唯一事实源，带成败）。
        super::central_events::record_for(
            self,
            &request.work_id,
            &request.execution_id,
            &request.session_id,
            if done {
                tiangong_central::event::WorkEventType::WorkItemCompleted
            } else {
                tiangong_central::event::WorkEventType::WorkItemFailed
            },
            if done { "工作完成" } else { "工作失败" },
        );
        // 持久化终态与成败（重启后徽章按 outcome 重建 completed/failed 分组）
        crate::central::mark_work_item_finished_with_session(
            self.core_manager.storage_root(),
            &request.work_id,
            Some(&request.session_id),
            !done,
        );
        let has_running = !self
            .central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if should_deliver_now(done, has_running) {
            self.deliver_pending_work_reports();
        }
        emit_central_work_updated_state(self);
    }
    /// 依赖终态后投递排队工作（在终态回调路径调用；递归走标准派发链）。
    /// `app`：Some（生产，投递经 spawn 走完整 dispatch 链）/ None（host
    /// 集成测试同步核心：仅取出匹配项并清排队标记，不实际投递）。
    pub(crate) fn flush_pending_dispatches(
        &self,
        finished_work_id: &str,
        finished_session_id: &str,
        app: Option<tauri::AppHandle>,
    ) {
        // 无句柄（host 集成测试同步核心）：先判空直接返回——队列与落盘
        // 标记保持原样，绝不「drain 后丢弃」（review：消费后 continue 是
        // 静默丢失，未来误传 None 会让排队工作无声消失且不重放）。
        if app.is_none() {
            return;
        }
        let ready: Vec<crate::central::CentralDispatchRequest> = {
            let mut pending = self
                .central_pending_dispatches
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (ready, waiting): (Vec<_>, Vec<_>) = pending.drain(..).partition(|req| {
                req.after_work_id.as_deref().is_some_and(|after| {
                    central_state::flush_matches(after, finished_work_id, finished_session_id)
                })
            });
            *pending = waiting;
            ready
        };
        for mut request in ready {
            // 清除排队落盘标记（已进入投递），再递归走标准链
            crate::central::clear_work_item_queued(
                self.core_manager.storage_root(),
                &request.session_id,
            );
            request.after_work_id = None;
            let app_handle = app.clone().expect("app 已在函数入口判空，此处必为 Some");
            tauri::async_runtime::spawn(async move {
                if let Err(error) =
                    handle_central_dispatch(state_of(&app_handle), &app_handle, request.clone())
                        .await
                {
                    tracing::warn!(%error, "排队工作投递未执行");
                    // 与派发路径同语义：投递未执行 ≠ 工作失败——不产生终态、
                    // 不伪装失败回报；注入说明（随下一条消息进入中枢对话）。
                    let state = state_of(&app_handle);
                    let notice = format!(
                        "[系统中枢] 排队工作 {} 投递未执行：{error}\n没有产生后台工作（不是工作失败）；依赖已终态，可重新派发。",
                        request.work_id
                    );
                    state.push_central_work_reports(vec![notice]);
                    super::events::emit_central_work_updated_state(state);
                }
            });
        }
    }

    /// 汇报已消费（注入或收集）：report_queued 转为 completed 保留
    /// （正常完成的工作留在清单可见），超出最近 10 个的旧 completed 移除。
    pub(crate) fn clear_queued_work_reports(&self) {
        let mut registry = self
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for entry in registry.iter_mut() {
            if entry.state == central_state::WorkState::ReportQueued {
                entry.state = central_state::WorkState::Completed;
            }
        }
        // 保留策略统一走 central-state（completed/failed 各留最近 10）
        let removals = central_state::retention_removals(&registry, 10);
        for index in removals.into_iter().rev() {
            registry.remove(index);
        }
    }

    /// 后台工作状态登记快照（前端状态条数据源）。
    pub(crate) fn central_work_status(&self) -> Vec<crate::central::CentralWorkStatus> {
        self.central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 注入全部暂存回报（第一条开启汇报轮，其余按 Core 原生引导排队）。
    pub(crate) fn deliver_pending_work_reports(&self) {
        use tiangong_core::agent_input::AgentInput;
        // 顺序（review k 项）：先确认 Core 存在再取走——Core 缺失时队列与
        // 状态登记都保持原样，由发送链路 flush 或 collect 兜底。
        let Some(core) = self
            .central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            tracing::warn!("中枢 Core 不存在，后台工作回报暂存待用户下一条消息");
            return;
        };
        let notices = self.take_central_work_reports();
        let mut failed: Vec<String> = Vec::new();
        for notice in notices {
            let prepared = vec![tiangong_types::ContentBlock::text(notice.clone())];
            if let Err(error) = core.deliver(AgentInputKind::prepared_with_id(
                scru128::new().to_string(),
                prepared,
            )) {
                tracing::warn!(%error, "后台工作回报注入中枢失败（放回队列）");
                failed.push(notice);
            }
        }
        // 投递失败的不丢弃：放回队列并保留待汇报登记（状态与队列一致）；
        // 全部成功才清除 report_queued 状态。
        if failed.is_empty() {
            self.clear_queued_work_reports();
        } else {
            self.central_work_reports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(failed);
        }
    }

    /// 取走该会话全部已挂载的终态回调（消费者终态触发用；表取模式使
    /// 任意消费者实例都能触发挂载的回调——见 commands.rs N1 注释）。
    /// 追加回报到暂存队列（投递失败的放回路径）。
    pub(crate) fn push_central_work_reports(&self, notices: Vec<String>) {
        self.central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(notices);
    }

    /// 取走全部暂存的后台工作回报（先进先出）。
    pub(crate) fn take_central_work_reports(&self) -> Vec<String> {
        let mut reports = self
            .central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *reports)
    }
}
