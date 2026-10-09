use std::collections::HashSet;

use crate::api::SharedAppContext;
use crate::webhook::model::{UpdateWebhookRequest, Webhook, WebhookRun, WebhookRunStatus};
use crate::webhook::store::WebhookStore;

/// 正在执行的 webhook id 集合，防止同一 webhook 重叠执行
static RUNNING: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// 执行 webhook 触发。
///
/// webhook 是 server 的固有能力：消息构造、会话解析、投递、执行记录全部由 server
/// 自包含完成，直接复用 [`ServerCoreBackend`] 投递消息，不经 scheduler 的执行抽象。
pub async fn execute_webhook(app_ctx: SharedAppContext, webhook: Webhook) {
    let store = match WebhookStore::open() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Webhook {} 打开 store 失败：{e}", webhook.id);
            return;
        }
    };

    let fresh = store.get(&webhook.id).ok().flatten().unwrap_or(webhook);

    // 防止同一 webhook 重叠执行
    {
        let mut running = RUNNING.lock().unwrap();
        if running.contains(&fresh.id) {
            tracing::warn!("Webhook {} 跳过：上一轮执行尚未完成", fresh.id);
            return;
        }
        running.insert(fresh.id.clone());
    }
    struct RunGuard(String);
    impl Drop for RunGuard {
        fn drop(&mut self) {
            RUNNING.lock().unwrap().remove(&self.0);
        }
    }
    let _guard = RunGuard(fresh.id.clone());

    let run_id = scru128::new().to_string();
    let now = chrono::Local::now().naive_local().to_string();

    // 解析 session：优先复用 webhook 绑定的会话，否则分配新 id。
    // 与 ServerSchedulerContext::resolve_session_id 同源：仅按磁盘落盘的会话判定存在性。
    let (session_id, created_new) = {
        let mut reuse = false;
        if let Some(sid) = fresh.session_id.as_deref() {
            let state = app_ctx.state.lock().await;
            if state.core_manager.session_exists(sid) {
                reuse = true;
            }
            if reuse {
                (sid.to_string(), false)
            } else {
                (scru128::new().to_string(), true)
            }
        } else {
            (scru128::new().to_string(), true)
        }
    };

    // 记录开始执行（本轮使用的 session，即便随后失败也保留痕迹）
    let run = WebhookRun {
        id: run_id.clone(),
        webhook_id: fresh.id.clone(),
        session_id: session_id.clone(),
        status: WebhookRunStatus::Running,
        started_at: now.clone(),
        finished_at: None,
        result_summary: None,
    };
    if let Err(e) = store.insert_run(&run) {
        tracing::error!("记录 WebhookRun 失败：{e}");
    }

    // 构造消息（webhook 专用头，前端据此与定时任务区分渲染）
    let message = format!(
        "[Webhook触发]\n任务名称：{}\n任务描述：{}\n\n{}",
        fresh.name, fresh.description, fresh.payload
    );

    // 投递消息到 Core（直接经 ServerCoreBackend，不经 scheduler）
    let result = app_ctx
        .core_backend
        .send_message_and_wait(&session_id, message, None, vec![], Default::default())
        .await;

    let finished_at = chrono::Local::now().naive_local().to_string();
    match result {
        Ok(_) => {
            // 投递成功后才把新 session_id 写回 webhook：失败时新会话很可能尚未落盘，
            // 立刻绑定会让后续触发因 session_exists==false 反复换新 id，彻底丢失关联。
            if created_new {
                let req = UpdateWebhookRequest {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                };
                if let Err(e) = store.update(&fresh.id, &req) {
                    tracing::warn!("Webhook {} 写回 session_id 失败：{e}", fresh.id);
                } else {
                    tracing::info!("Webhook {} 已绑定会话 {}", fresh.id, session_id);
                }
            }
            let _ = store.update_run_status(
                &run_id,
                &fresh.id,
                &WebhookRunStatus::Succeeded,
                Some(&finished_at),
                Some("消息已发送至会话"),
            );
            tracing::info!("Webhook {} 消息已发送", fresh.id);
        }
        Err(e) => {
            let err_msg = format!("发送失败：{e}");
            let _ = store.update_run_status(
                &run_id,
                &fresh.id,
                &WebhookRunStatus::Failed,
                Some(&finished_at),
                Some(&err_msg),
            );
            tracing::error!("Webhook {} 发送失败：{e}", fresh.id);
        }
    }
}
