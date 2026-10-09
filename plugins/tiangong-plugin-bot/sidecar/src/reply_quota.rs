//! 回复窗口额度：bot 侧自主限流、聚合与拆分（issue #572）。
//!
//! QQ / 微信的被动回复依附于用户最近一条消息，平台对每条消息的可回复次数和
//! 有效期有限制。回复改由 Agent 经 MCP 发送后，Agent 可能在一轮中多次发送，
//! 必须由 bot 统一管理额度，避免进展消息耗尽额度导致最终答复发不出去。
//!
//! 本模块只包含纯逻辑，状态持久化由 `target_store` 在跨进程文件锁内完成
//! （MCP 为 stdio 子进程，每次调用可能是新进程，无法依赖内存状态）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// 单个回复窗口的平台限制与 bot 侧安全策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaPolicy {
    /// 每条入站消息最多可回复的消息条数。
    pub max_replies: u32,
    /// 回复窗口有效期（秒，已扣除安全余量）；`None` 表示平台未公开，不在本地判断过期。
    pub window_secs: Option<i64>,
    /// 两条进展消息之间的最小间隔（秒）；间隔内的进展会被暂存并合并。
    pub progress_interval_secs: i64,
    /// 单条消息的最大字符数，超出时拆分。
    pub segment_chars: usize,
    /// 暂存进展的最大条数，超出时丢弃最早的。
    pub max_pending_progress: usize,
}

/// 发送类型：进展消息必须为最终答复预留至少 1 个额度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    Progress,
    Final,
}

/// 持久化的窗口状态，挂在推送目标记录上。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaState {
    /// 当前窗口对应的入站上下文（QQ 为 msg_id，微信为 context_token 的摘要）。
    #[serde(default)]
    pub context: String,
    #[serde(default, with = "naive_time_opt")]
    pub window_started_at: Option<NaiveDateTime>,
    #[serde(default)]
    pub used: u32,
    #[serde(default, with = "naive_time_opt")]
    pub last_progress_at: Option<NaiveDateTime>,
    #[serde(default)]
    pub pending_progress: Vec<String>,
}

/// 文本发送的规划结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPlan {
    /// 立即发送这些分段；`first_seq` 为第一段占用的额度序号（从 1 开始）。
    Send {
        segments: Vec<String>,
        first_seq: u32,
    },
    /// 进展消息已暂存，将合并到下一条进展中发送。
    Queued { pending: usize },
    /// 拒绝发送，附带给 Agent 的说明。
    Rejected(String),
}

/// 单条媒体发送的规划结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPlan {
    Send { seq: u32 },
    Rejected(String),
}

impl QuotaState {
    /// 收到新的入站消息时开启新窗口；同一上下文重复推送时保持原状态。
    pub fn observe_inbound(&mut self, context: &str, now: NaiveDateTime) {
        if self.context == context && self.window_started_at.is_some() {
            return;
        }
        *self = Self {
            context: context.to_string(),
            window_started_at: Some(now),
            ..Self::default()
        };
    }

    fn remaining(&self, policy: &QuotaPolicy) -> u32 {
        policy.max_replies.saturating_sub(self.used)
    }

    fn check_window(&self, policy: &QuotaPolicy, now: NaiveDateTime) -> Result<(), String> {
        let Some(started) = self.window_started_at else {
            return Err("尚未收到该会话的消息，没有可用的回复窗口".to_string());
        };
        if let Some(window) = policy.window_secs
            && (now - started).num_seconds() >= window
        {
            return Err("回复窗口已过期，需等待用户发送新消息后才能继续回复".to_string());
        }
        Ok(())
    }

    /// 规划一次文本发送，成功时立即占用额度（调用方须在文件锁内调用并落盘）。
    pub fn plan_text(
        &mut self,
        policy: &QuotaPolicy,
        kind: ReplyKind,
        text: &str,
        now: NaiveDateTime,
    ) -> TextPlan {
        if let Err(reason) = self.check_window(policy, now) {
            return TextPlan::Rejected(reason);
        }
        let remaining = self.remaining(policy);
        match kind {
            ReplyKind::Final => {
                let segments = split_text(text, policy.segment_chars);
                if remaining == 0 {
                    return TextPlan::Rejected("本条消息的回复额度已用完，无法再发送".to_string());
                }
                if segments.len() as u32 > remaining {
                    return TextPlan::Rejected(format!(
                        "最终答复需要拆成 {} 条消息，但仅剩 {remaining} 次回复额度；请精简到约 {} 字以内后重新发送",
                        segments.len(),
                        remaining as usize
                            * content_chars(policy.segment_chars, remaining as usize)
                    ));
                }
                // 最终答复已覆盖进展，未发出的暂存进展不再发送。
                self.pending_progress.clear();
                let first_seq = self.used + 1;
                self.used += segments.len() as u32;
                TextPlan::Send {
                    segments,
                    first_seq,
                }
            }
            ReplyKind::Progress => {
                if remaining <= 1 {
                    return TextPlan::Rejected(format!(
                        "回复额度仅剩 {remaining} 次，需保留给最终答复，本条进展未发送；请把进展汇总进最终答复（is_final=true）"
                    ));
                }
                let throttled = self
                    .last_progress_at
                    .is_some_and(|last| (now - last).num_seconds() < policy.progress_interval_secs);
                if throttled {
                    self.pending_progress.push(text.to_string());
                    let overflow = self
                        .pending_progress
                        .len()
                        .saturating_sub(policy.max_pending_progress);
                    self.pending_progress.drain(..overflow);
                    return TextPlan::Queued {
                        pending: self.pending_progress.len(),
                    };
                }
                let mut parts = std::mem::take(&mut self.pending_progress);
                parts.push(text.to_string());
                let merged = truncate_chars(&parts.join("\n"), policy.segment_chars);
                self.last_progress_at = Some(now);
                self.used += 1;
                TextPlan::Send {
                    segments: vec![merged],
                    first_seq: self.used,
                }
            }
        }
    }

    /// 规划一次图片/文件发送：每个文件占 1 个额度，非最终发送同样须为最终答复预留额度。
    pub fn plan_media(
        &mut self,
        policy: &QuotaPolicy,
        kind: ReplyKind,
        now: NaiveDateTime,
    ) -> MediaPlan {
        if let Err(reason) = self.check_window(policy, now) {
            return MediaPlan::Rejected(reason);
        }
        let remaining = self.remaining(policy);
        let reserve = match kind {
            ReplyKind::Final => 0,
            ReplyKind::Progress => 1,
        };
        if remaining <= reserve {
            return MediaPlan::Rejected(match kind {
                ReplyKind::Final => "本条消息的回复额度已用完，无法再发送".to_string(),
                ReplyKind::Progress => format!(
                    "回复额度仅剩 {remaining} 次，需保留给最终答复；如该文件就是最终答复的一部分，请设置 is_final=true"
                ),
            });
        }
        self.used += 1;
        MediaPlan::Send { seq: self.used }
    }
}

/// 按字符数拆分文本，优先在换行处断开；多段时在末尾追加 `(i/n)` 标记。
pub fn split_text(text: &str, segment_chars: usize) -> Vec<String> {
    let segment_chars = segment_chars.max(16);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= segment_chars {
        return vec![text.to_string()];
    }
    // 为分段标记预留空间，标记最长形如 " (99/99)"。
    let body_chars = segment_chars - 8;
    let mut segments = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + body_chars).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len()
            && let Some(offset) = chars[start..hard_end].iter().rposition(|c| *c == '\n')
            && offset >= body_chars / 2
        {
            end = start + offset + 1;
        }
        let segment: String = chars[start..end].iter().collect();
        segments.push(segment.trim_end().to_string());
        start = end;
    }
    let total = segments.len();
    segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| format!("{segment} ({}/{total})", index + 1))
        .collect()
}

/// 剩余 `segments` 段时每段可容纳的正文字符数（多段需扣除分段标记）。
fn content_chars(segment_chars: usize, segments: usize) -> usize {
    if segments <= 1 {
        segment_chars
    } else {
        segment_chars.max(16) - 8
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

mod naive_time_opt {
    use chrono::NaiveDateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    const FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

    pub fn serialize<S: Serializer>(
        value: &Option<NaiveDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(time) => serializer.serialize_some(&time.format(FORMAT).to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<NaiveDateTime>, D::Error> {
        let value = Option::<String>::deserialize(deserializer)?;
        Ok(value.and_then(|text| NaiveDateTime::parse_from_str(&text, FORMAT).ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, NaiveDate};

    const POLICY: QuotaPolicy = QuotaPolicy {
        max_replies: 4,
        window_secs: Some(3600 - 60),
        progress_interval_secs: 30,
        segment_chars: 40,
        max_pending_progress: 3,
    };

    fn t0() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 4)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn opened() -> QuotaState {
        let mut state = QuotaState::default();
        state.observe_inbound("msg-1", t0());
        state
    }

    #[test]
    fn progress_always_keeps_one_slot_for_final_reply() {
        let mut state = opened();
        let mut now = t0();
        for _ in 0..3 {
            assert!(matches!(
                state.plan_text(&POLICY, ReplyKind::Progress, "进展", now),
                TextPlan::Send { .. }
            ));
            now += Duration::seconds(60);
        }
        assert!(matches!(
            state.plan_text(&POLICY, ReplyKind::Progress, "进展", now),
            TextPlan::Rejected(reason) if reason.contains("保留给最终答复")
        ));
        assert_eq!(
            state.plan_text(&POLICY, ReplyKind::Final, "完成", now),
            TextPlan::Send {
                segments: vec!["完成".to_string()],
                first_seq: 4
            }
        );
        assert!(matches!(
            state.plan_text(&POLICY, ReplyKind::Final, "再来", now),
            TextPlan::Rejected(reason) if reason.contains("已用完")
        ));
    }

    #[test]
    fn throttled_progress_is_queued_and_merged_into_next_progress() {
        let mut state = opened();
        let now = t0();
        assert!(matches!(
            state.plan_text(&POLICY, ReplyKind::Progress, "开始", now),
            TextPlan::Send { first_seq: 1, .. }
        ));
        assert_eq!(
            state.plan_text(
                &POLICY,
                ReplyKind::Progress,
                "截图中",
                now + Duration::seconds(5)
            ),
            TextPlan::Queued { pending: 1 }
        );
        assert_eq!(
            state.plan_text(
                &POLICY,
                ReplyKind::Progress,
                "分析中",
                now + Duration::seconds(40)
            ),
            TextPlan::Send {
                segments: vec!["截图中\n分析中".to_string()],
                first_seq: 2
            }
        );
        assert_eq!(state.used, 2);
        assert!(state.pending_progress.is_empty());
    }

    #[test]
    fn final_reply_discards_pending_progress() {
        let mut state = opened();
        let now = t0();
        state.plan_text(&POLICY, ReplyKind::Progress, "开始", now);
        state.plan_text(
            &POLICY,
            ReplyKind::Progress,
            "进行中",
            now + Duration::seconds(1),
        );
        assert_eq!(state.pending_progress.len(), 1);
        assert!(matches!(
            state.plan_text(
                &POLICY,
                ReplyKind::Final,
                "结果",
                now + Duration::seconds(2)
            ),
            TextPlan::Send { first_seq: 2, .. }
        ));
        assert!(state.pending_progress.is_empty());
    }

    #[test]
    fn pending_progress_is_bounded() {
        let mut state = opened();
        let now = t0();
        state.plan_text(&POLICY, ReplyKind::Progress, "0", now);
        for index in 1..=5 {
            state.plan_text(&POLICY, ReplyKind::Progress, &index.to_string(), now);
        }
        assert_eq!(state.pending_progress, vec!["3", "4", "5"]);
    }

    #[test]
    fn long_final_reply_is_split_and_rejected_when_quota_is_short() {
        let mut state = opened();
        let long = "甲".repeat(70);
        match state.plan_text(&POLICY, ReplyKind::Final, &long, t0()) {
            TextPlan::Send {
                segments,
                first_seq,
            } => {
                assert_eq!(first_seq, 1);
                assert_eq!(segments.len(), 3);
                assert!(segments[0].ends_with("(1/3)"));
                assert!(segments.iter().all(|s| s.chars().count() <= 40));
            }
            other => panic!("expected send, got {other:?}"),
        }
        let too_long = "乙".repeat(200);
        assert!(matches!(
            state.plan_text(&POLICY, ReplyKind::Final, &too_long, t0()),
            TextPlan::Rejected(reason) if reason.contains("仅剩 1 次")
        ));
        assert_eq!(state.used, 3, "拒绝时不得占用额度");
    }

    #[test]
    fn expired_window_rejects_everything() {
        let mut state = opened();
        let late = t0() + Duration::seconds(3600);
        assert!(matches!(
            state.plan_text(&POLICY, ReplyKind::Final, "迟到", late),
            TextPlan::Rejected(reason) if reason.contains("已过期")
        ));
        assert!(matches!(
            state.plan_media(&POLICY, ReplyKind::Final, late),
            MediaPlan::Rejected(_)
        ));
    }

    #[test]
    fn new_inbound_resets_window_but_duplicate_push_does_not() {
        let mut state = opened();
        state.plan_text(&POLICY, ReplyKind::Final, "a", t0());
        state.observe_inbound("msg-1", t0() + Duration::seconds(10));
        assert_eq!(state.used, 1, "同一消息重复推送不得重置额度");
        state.observe_inbound("msg-2", t0() + Duration::seconds(20));
        assert_eq!(state.used, 0);
        assert_eq!(state.context, "msg-2");
    }

    #[test]
    fn media_reserves_final_slot_unless_final() {
        let mut state = opened();
        state.used = 3;
        assert!(matches!(
            state.plan_media(&POLICY, ReplyKind::Progress, t0()),
            MediaPlan::Rejected(reason) if reason.contains("is_final=true")
        ));
        assert_eq!(
            state.plan_media(&POLICY, ReplyKind::Final, t0()),
            MediaPlan::Send { seq: 4 }
        );
    }

    #[test]
    fn missing_window_is_rejected_and_unknown_window_never_expires() {
        let mut state = QuotaState::default();
        assert!(matches!(
            state.plan_text(&POLICY, ReplyKind::Final, "x", t0()),
            TextPlan::Rejected(_)
        ));
        let policy = QuotaPolicy {
            window_secs: None,
            ..POLICY
        };
        let mut state = opened();
        assert!(matches!(
            state.plan_text(&policy, ReplyKind::Final, "x", t0() + Duration::days(3)),
            TextPlan::Send { .. }
        ));
    }

    #[test]
    fn state_round_trips_through_json_and_defaults_for_old_records() {
        let mut state = opened();
        state.plan_text(&POLICY, ReplyKind::Progress, "p", t0());
        let json = serde_json::to_string(&state).unwrap();
        let restored: QuotaState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);
        let old: QuotaState = serde_json::from_str("{}").unwrap();
        assert_eq!(old, QuotaState::default());
    }
}
