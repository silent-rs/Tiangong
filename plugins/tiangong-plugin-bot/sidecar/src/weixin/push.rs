//! 微信推送：使用最近一条入站消息的回复上下文（`context_token`）发送文本与文件。
//!
//! 每条入站消息的回复次数有限，由回复额度规划合并过频进展、拆分超长最终答复，
//! 并始终为最终答复保留额度。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chrono::Local;

use super::connection::BotState;
use super::ilink;
use super::target_store::{self, AuthorizedTarget};
use crate::delivery::{
    self, DeliveryClaim, DeliveryResult, MAX_TEXT_CHARS, SendFailure, finish_delivery,
    finish_queued, partial_failure, rejected,
};
use crate::local_media::read_local_media;
use crate::reply_quota::{MediaPlan, ReplyKind, TextPlan};
use crate::tools::{SendLocalMediaInput, SendTextInput};

const WINDOW_REPLACED: &str =
    "推送目标不存在、已被删除，或用户已发送新消息开启新的回复窗口；请重新确认后再发送";

enum Resolved {
    Ready(AuthorizedTarget),
    Return(DeliveryResult),
}

fn resolve_target(data_dir: &Path, target_id: &str) -> Result<Resolved> {
    let Some(target) = target_store::find_authorized(data_dir, target_id)? else {
        return Ok(Resolved::Return(rejected("推送目标不存在或尚未授权")));
    };
    if target.to_user_id.trim().is_empty() || target.context_token.trim().is_empty() {
        return Ok(Resolved::Return(rejected(
            "缺少最近微信消息的回复上下文，请先从移动端发送一条消息",
        )));
    }
    Ok(Resolved::Ready(target))
}

fn reply_kind(is_final: bool) -> ReplyKind {
    if is_final {
        ReplyKind::Final
    } else {
        ReplyKind::Progress
    }
}

fn classify_platform_error(action: &str, error: anyhow::Error) -> SendFailure {
    let detail = format!("{error:#}");
    tracing::warn!("微信推送{action}失败: {detail}");
    if detail.contains("iLink sendmessage 失败")
        || detail.contains("iLink getuploadurl 失败")
        || detail.contains("CDN 上传被拒绝")
        || detail.contains("iLink 请求失败（HTTP 4")
    {
        SendFailure::Rejected(format!(
            "微信平台拒绝{action}，最近消息的回复上下文可能已失效"
        ))
    } else {
        SendFailure::Unknown(format!("微信{action}结果未知，未自动重试"))
    }
}

pub(crate) async fn send_text_message(
    state: &Arc<BotState>,
    input: &SendTextInput,
) -> Result<DeliveryResult> {
    let text = input.text.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        return Ok(rejected("消息不能为空且不能超过 4000 个字符"));
    }
    let target = match resolve_target(&state.data_dir, input.target_id.trim())? {
        Resolved::Ready(target) => target,
        Resolved::Return(result) => return Ok(result),
    };
    let (path, record) = match delivery::claim_delivery(
        &state.data_dir,
        &target.target_id,
        input.idempotency_key.trim(),
    )? {
        DeliveryClaim::Existing(result) => return Ok(result),
        DeliveryClaim::New { path, delivery } => (path, delivery),
    };
    let kind = reply_kind(input.is_final);
    let now = Local::now().naive_local();
    let plan = target_store::update_quota(
        &state.data_dir,
        &target.target_id,
        &target.context_token,
        |policy, quota| quota.plan_text(policy, kind, text, now),
    )?
    .unwrap_or_else(|| TextPlan::Rejected(WINDOW_REPLACED.to_string()));
    let segments = match plan {
        TextPlan::Send { segments, .. } => segments,
        TextPlan::Queued { pending } => return Ok(finish_queued(&path, record, pending)),
        TextPlan::Rejected(reason) => {
            return Ok(finish_delivery(
                &path,
                record,
                Err(SendFailure::Rejected(reason)),
                "",
            ));
        }
    };
    let total = segments.len();
    let mut outcome = Ok(None);
    for (index, segment) in segments.iter().enumerate() {
        let sent = ilink::send_message(
            &state.http,
            &state.api_base_url,
            &state.bot_token,
            &target.to_user_id,
            &target.context_token,
            segment,
        )
        .await;
        if let Err(error) = sent {
            let failure = classify_platform_error("文本发送", error);
            outcome = Err(partial_failure(failure, index, total));
            break;
        }
    }
    let accepted = if total > 1 {
        format!("微信已受理文本消息（已拆分为 {total} 条发送）")
    } else {
        "微信已受理文本消息（受最近消息回复窗口限制）".to_string()
    };
    Ok(finish_delivery(&path, record, outcome, &accepted))
}

pub(crate) async fn send_media_message(
    state: &Arc<BotState>,
    input: &SendLocalMediaInput,
    workspace: Option<&Path>,
    image: bool,
) -> Result<DeliveryResult> {
    let label = if image { "图片" } else { "文件" };
    let target = match resolve_target(&state.data_dir, input.target_id.trim())? {
        Resolved::Ready(target) => target,
        Resolved::Return(result) => return Ok(result),
    };
    let media = match read_local_media(input.file_path.trim(), workspace, image) {
        Ok(media) => media,
        Err(message) => return Ok(rejected(&message)),
    };
    let (path, record) = match delivery::claim_delivery(
        &state.data_dir,
        &target.target_id,
        input.idempotency_key.trim(),
    )? {
        DeliveryClaim::Existing(result) => return Ok(result),
        DeliveryClaim::New { path, delivery } => (path, delivery),
    };
    let kind = reply_kind(input.is_final);
    let now = Local::now().naive_local();
    let plan = target_store::update_quota(
        &state.data_dir,
        &target.target_id,
        &target.context_token,
        |policy, quota| quota.plan_media(policy, kind, now),
    )?
    .unwrap_or_else(|| MediaPlan::Rejected(WINDOW_REPLACED.to_string()));
    if let MediaPlan::Rejected(reason) = plan {
        return Ok(finish_delivery(
            &path,
            record,
            Err(SendFailure::Rejected(reason)),
            "",
        ));
    }
    let outcome = ilink::send_local_file(
        &state.http,
        &state.api_base_url,
        &state.bot_token,
        &target.to_user_id,
        &target.context_token,
        &media.path,
        "",
    )
    .await
    .map(|()| None)
    .map_err(|error| classify_platform_error(&format!("{label}发送"), error));
    Ok(finish_delivery(
        &path,
        record,
        outcome,
        &format!("微信已受理{label}消息（受最近消息回复窗口限制）"),
    ))
}
