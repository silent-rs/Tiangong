//! QQ 推送：使用最近一条入站消息的回复上下文（`msg_id`）发送文本与富媒体。
//!
//! QQ 被动回复次数有限（单聊 4 次 / 群聊 5 次），由回复额度规划合并过频进展、
//! 拆分超长最终答复，并始终为最终答复保留额度。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chrono::Local;

use super::connection::{BotState, ReplyTarget, send_local_file, send_text};
use super::target_store::{self, AuthorizedTarget};
use crate::delivery::{
    self, DeliveryClaim, DeliveryResult, MAX_TEXT_CHARS, SendFailure, finish_delivery,
    finish_queued, partial_failure, rejected,
};
use crate::local_media::read_local_media;
use crate::reply_quota::{MediaPlan, ReplyKind, TextPlan};
use crate::tools::{SendLocalMediaInput, SendTextInput};

/// 推送占用的 `msg_seq` 基数：与同步回复路径（从 1 递增）错开。
///
/// QQ 要求同一 `msg_id` 下 `msg_seq` 不重复；额度序号在窗口内单调递增，
/// 加上基数后即可保证推送之间、以及与同步回复之间都不冲突。
const PUSH_MESSAGE_SEQUENCE_START: u32 = 1000;
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
    if target.recipient_id.trim().is_empty() || target.message_id.trim().is_empty() {
        return Ok(Resolved::Return(rejected(
            "缺少最近 QQ 消息的回复上下文，请先从移动端发送一条消息",
        )));
    }
    Ok(Resolved::Ready(target))
}

fn reply_target(target: &AuthorizedTarget) -> ReplyTarget {
    if target.kind == "group" {
        ReplyTarget::Group(target.recipient_id.clone())
    } else {
        ReplyTarget::User(target.recipient_id.clone())
    }
}

fn message_sequence(slot: u32) -> u32 {
    PUSH_MESSAGE_SEQUENCE_START + slot
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
    tracing::warn!("QQ 推送{action}失败: {detail}");
    if detail.contains("业务失败")
        || detail.contains("失败（HTTP 4")
        || detail.contains("上传被拒绝")
    {
        SendFailure::Rejected(format!(
            "QQ 平台拒绝{action}，最近消息的回复上下文可能已失效"
        ))
    } else {
        SendFailure::Unknown(format!("QQ {action}结果未知，未自动重试"))
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
        &target.message_id,
        |policy, quota| quota.plan_text(policy, kind, text, now),
    )?
    .unwrap_or_else(|| TextPlan::Rejected(WINDOW_REPLACED.to_string()));
    let (segments, first_seq) = match plan {
        TextPlan::Send {
            segments,
            first_seq,
        } => (segments, first_seq),
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

    let platform_target = reply_target(&target);
    let total = segments.len();
    let mut outcome = Ok(None);
    for (index, segment) in segments.iter().enumerate() {
        let sent = send_text(
            state,
            &platform_target,
            &target.message_id,
            message_sequence(first_seq + index as u32),
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
        format!("QQ 已受理文本消息（已拆分为 {total} 条发送）")
    } else {
        "QQ 已受理文本消息（受最近消息回复窗口限制）".to_string()
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
        &target.message_id,
        |policy, quota| quota.plan_media(policy, kind, now),
    )?
    .unwrap_or_else(|| MediaPlan::Rejected(WINDOW_REPLACED.to_string()));
    let seq = match plan {
        MediaPlan::Send { seq } => seq,
        MediaPlan::Rejected(reason) => {
            return Ok(finish_delivery(
                &path,
                record,
                Err(SendFailure::Rejected(reason)),
                "",
            ));
        }
    };
    let outcome = send_local_file(
        state,
        &reply_target(&target),
        &target.message_id,
        message_sequence(seq),
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
        &format!("QQ 已受理{label}消息（受最近消息回复窗口限制）"),
    ))
}
