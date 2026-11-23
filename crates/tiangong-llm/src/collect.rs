//! 所有适配器共享的事件组装；不认识 SDK、会话或工具执行器。
use crate::{
    error::LlmError,
    message::{ChatMessage, MessageContent, MessageRole, ThinkingContent},
    response::{ProviderResponse, StopReason},
    stream::{CallKey, ProviderStream, ProviderStreamEvent},
    tool::ToolCall,
    usage::TokenUsageData,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionStatus {
    Complete,
    Incomplete,
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct CollectedResponse {
    pub response: ProviderResponse,
    pub status: CompletionStatus,
    pub error: Option<LlmError>,
    pub invalid_tool_calls: Vec<InvalidCall>,
}

#[derive(Debug, Clone)]
pub struct InvalidCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub reason: String,
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    finished: bool,
    snapshot: Option<String>,
    diagnostic: Option<String>,
}

#[derive(Default)]
pub struct ResponseAccumulator {
    pub text: String,
    pub reasoning: String,
    pub signature: Option<String>,
    pub usage: Option<TokenUsageData>,
    pub stop_reason: Option<StopReason>,
    pub error: Option<LlmError>,
    calls: BTreeMap<CallKey, PendingCall>,
    redacted: Vec<String>,
    ended: bool,
}

impl ResponseAccumulator {
    pub fn push(&mut self, event: ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::TextDelta(text) => self.text.push_str(&text),
            ProviderStreamEvent::ReasoningDelta(text) => self.reasoning.push_str(&text),
            ProviderStreamEvent::ReasoningSignatureDelta(signature) => {
                self.signature.get_or_insert_default().push_str(&signature)
            }
            ProviderStreamEvent::RedactedThinking(data) => self.redacted.push(data),
            ProviderStreamEvent::ToolCallMetadata { key, id, name } => {
                let call = self.calls.entry(key).or_default();
                if !id.trim().is_empty() {
                    call.id = id;
                }
                if !name.trim().is_empty() {
                    call.name = name;
                }
            }
            ProviderStreamEvent::ToolArgumentsDelta { key, fragment } => {
                let call = self.calls.entry(key).or_default();
                if call.finished {
                    call.diagnostic = Some("工具结束后仍收到参数增量".into());
                } else {
                    call.arguments.push_str(&fragment);
                }
            }
            ProviderStreamEvent::ToolArgumentsSnapshot { key, arguments } => {
                let call = self.calls.entry(key).or_default();
                // Snapshot 是权威终态，不与之前 delta 拼接；重复快照幂等。
                if let Some(previous) = &call.snapshot
                    && previous != &arguments
                {
                    call.diagnostic = Some("工具终态参数快照冲突".into());
                }
                call.snapshot = Some(arguments);
            }
            ProviderStreamEvent::ToolCallFinished { key } => {
                self.calls.entry(key).or_default().finished = true
            }
            ProviderStreamEvent::Usage(next) => {
                let usage = self.usage.get_or_insert_default();
                usage.prompt_tokens = usage.prompt_tokens.max(next.prompt_tokens);
                usage.completion_tokens = usage.completion_tokens.max(next.completion_tokens);
                usage.total_tokens = usage
                    .total_tokens
                    .max(next.total_tokens)
                    .max(usage.prompt_tokens + usage.completion_tokens);
                if next.prompt_cache_hit_tokens.is_some() {
                    usage.prompt_cache_hit_tokens = next.prompt_cache_hit_tokens;
                }
                if next.prompt_cache_miss_tokens.is_some() {
                    usage.prompt_cache_miss_tokens = next.prompt_cache_miss_tokens;
                }
            }
            ProviderStreamEvent::MessageEnd { stop_reason } => {
                self.ended = true;
                if stop_reason.is_some() {
                    self.stop_reason = stop_reason;
                }
            }
            ProviderStreamEvent::Error(message) => self.error = Some(LlmError::Stream(message)),
            ProviderStreamEvent::MessageStart => {}
        }
    }

    pub fn finish(mut self) -> CollectedResponse {
        let status = if self.error.is_some() || !self.ended {
            CompletionStatus::Interrupted
        } else if matches!(
            self.stop_reason,
            Some(StopReason::MaxTokens | StopReason::Other(_))
        ) {
            CompletionStatus::Incomplete
        } else {
            CompletionStatus::Complete
        };
        if !self.ended && self.error.is_none() {
            self.error = Some(LlmError::Stream("流缺少合法终态".into()));
        }
        let mut content = Vec::new();
        if !self.reasoning.is_empty() || self.signature.is_some() {
            content.push(MessageContent::Thinking(ThinkingContent {
                thinking: self.reasoning.clone(),
                signature: self.signature,
            }));
        }
        content.extend(
            self.redacted
                .into_iter()
                .map(MessageContent::RedactedThinking),
        );
        if !self.text.is_empty() {
            content.push(MessageContent::Text(self.text));
        }
        let mut invalid = Vec::new();
        for (key, call) in self.calls {
            let arguments = call.snapshot.as_deref().unwrap_or(&call.arguments);
            let parsed = if arguments.trim().is_empty() && call.finished {
                Ok(serde_json::json!({}))
            } else {
                serde_json::from_str(arguments)
            };
            let reason = if let Some(diagnostic) = call.diagnostic {
                Some(diagnostic)
            } else if call.name.trim().is_empty() {
                Some("工具名称缺失".to_owned())
            } else if !call.finished && status != CompletionStatus::Complete {
                Some("工具调用未完整结束".to_owned())
            } else {
                parsed.as_ref().err().map(|error| error.to_string())
            };
            let id = if call.id.trim().is_empty() {
                format!("item_{}", key.0)
            } else {
                call.id
            };
            if let Some(reason) = reason {
                invalid.push(InvalidCall {
                    id,
                    name: call.name,
                    arguments: arguments.to_owned(),
                    reason,
                });
            } else if let Ok(arguments) = parsed {
                content.push(MessageContent::ToolCall(ToolCall {
                    id,
                    name: call.name,
                    arguments,
                }));
            }
        }
        CollectedResponse {
            response: ProviderResponse {
                id: None,
                model: None,
                assistant_message: ChatMessage::new(MessageRole::Assistant, content),
                reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
                stop_reason: self.stop_reason,
                usage: self.usage,
                raw: None,
            },
            status,
            error: self.error,
            invalid_tool_calls: invalid,
        }
    }
}

pub async fn collect_response(mut stream: ProviderStream) -> CollectedResponse {
    let mut state = ResponseAccumulator::default();
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => state.push(event),
            Err(error) => {
                state.error = Some(error);
                break;
            }
        }
        if state.error.is_some() {
            break;
        }
    }
    state.finish()
}

pub async fn collect_text(
    stream: ProviderStream,
) -> Result<(String, Option<TokenUsageData>), LlmError> {
    let collected = collect_response(stream).await;
    if let Some(error) = collected.error {
        return Err(error);
    }
    if collected.status != CompletionStatus::Complete || !collected.invalid_tool_calls.is_empty() {
        return Err(LlmError::Stream("文本生成未完整结束".into()));
    }
    let mut text = String::new();
    for block in collected.response.assistant_message.content {
        match block {
            MessageContent::Text(part) => text.push_str(&part),
            MessageContent::ToolCall(_) => {
                return Err(LlmError::InvalidRequest(
                    "纯文本生成意外返回工具调用".into(),
                ));
            }
            _ => {}
        }
    }
    Ok((text, collected.response.usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CallKey;
    fn metadata(key: u64) -> ProviderStreamEvent {
        ProviderStreamEvent::ToolCallMetadata {
            key: CallKey(key),
            id: "same".into(),
            name: "read_file".into(),
        }
    }
    fn snapshot(key: u64, arguments: &str) -> ProviderStreamEvent {
        ProviderStreamEvent::ToolArgumentsSnapshot {
            key: CallKey(key),
            arguments: arguments.into(),
        }
    }
    fn done(key: u64) -> ProviderStreamEvent {
        ProviderStreamEvent::ToolCallFinished { key: CallKey(key) }
    }
    fn ended() -> ProviderStreamEvent {
        ProviderStreamEvent::MessageEnd {
            stop_reason: Some(StopReason::ToolUse),
        }
    }
    fn calls(result: &CollectedResponse) -> Vec<&ToolCall> {
        result
            .response
            .assistant_message
            .content
            .iter()
            .filter_map(|block| match block {
                MessageContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }
    #[test]
    fn repeated_ids_and_repeated_metadata_do_not_overwrite_calls() {
        let mut state = ResponseAccumulator::default();
        for event in [
            metadata(0),
            snapshot(0, r#"{"path":"a"}"#),
            done(0),
            metadata(0),
            metadata(1),
            snapshot(1, r#"{"path":"b"}"#),
            done(1),
            ended(),
        ] {
            state.push(event);
        }
        let result = state.finish();
        assert_eq!(result.status, CompletionStatus::Complete);
        let calls = calls(&result);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a");
        assert_eq!(calls[1].arguments["path"], "b");
        assert_eq!(calls[0].id, calls[1].id, "历史唯一化不是 collector 职责");
    }
    #[test]
    fn snapshot_replaces_deltas_and_duplicates_are_idempotent() {
        let mut state = ResponseAccumulator::default();
        for event in [
            ProviderStreamEvent::ToolArgumentsDelta {
                key: CallKey(0),
                fragment: "{\"path\":".into(),
            },
            metadata(0),
            snapshot(0, r#"{"path":"a"}"#),
            done(0),
            snapshot(0, r#"{"path":"a"}"#),
            done(0),
            ended(),
        ] {
            state.push(event);
        }
        let result = state.finish();
        assert!(result.invalid_tool_calls.is_empty());
        assert_eq!(calls(&result)[0].arguments, serde_json::json!({"path":"a"}));
    }
    #[test]
    fn malformed_or_conflicting_snapshot_never_executes() {
        for second in ["{", r#"{"path":"b"}"#] {
            let mut state = ResponseAccumulator::default();
            for event in [
                metadata(0),
                snapshot(0, r#"{"path":"a"}"#),
                done(0),
                snapshot(0, second),
                ended(),
            ] {
                state.push(event);
            }
            let result = state.finish();
            assert!(calls(&result).is_empty());
            assert_eq!(result.invalid_tool_calls.len(), 1);
        }
    }
    #[tokio::test]
    async fn strict_text_rejects_eof_truncation_errors_and_tools() {
        let cases = vec![
            vec![Ok(ProviderStreamEvent::TextDelta("partial".into()))],
            vec![
                Ok(ProviderStreamEvent::TextDelta("partial".into())),
                Ok(ProviderStreamEvent::MessageEnd {
                    stop_reason: Some(StopReason::MaxTokens),
                }),
            ],
            vec![
                Ok(ProviderStreamEvent::TextDelta("partial".into())),
                Err(LlmError::Transport("断开".into())),
            ],
            vec![
                Ok(metadata(0)),
                Ok(snapshot(0, "{}")),
                Ok(done(0)),
                Ok(ended()),
            ],
        ];
        for events in cases {
            assert!(
                collect_text(Box::pin(futures_util::stream::iter(events)))
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn text_collector_keeps_usage_after_terminal_and_missing_usage_is_none() {
        let events = vec![
            Ok(ProviderStreamEvent::TextDelta("title".into())),
            Ok(ended()),
            Ok(ProviderStreamEvent::Usage(TokenUsageData::new(3, 2))),
        ];
        let (text, usage) = collect_text(Box::pin(futures_util::stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(text, "title");
        assert_eq!(usage.unwrap().total_tokens, 5);
        let (_, usage) = collect_text(Box::pin(futures_util::stream::iter(vec![Ok(ended())])))
            .await
            .unwrap();
        assert!(usage.is_none());
    }
}

#[cfg(test)]
mod semantic_tests {
    use super::*;
    #[test]
    fn snapshot_signature_and_redacted_data_are_preserved() {
        let mut state = ResponseAccumulator::default();
        state.push(ProviderStreamEvent::ReasoningDelta("thought".into()));
        state.push(ProviderStreamEvent::ReasoningSignatureDelta("sig".into()));
        state.push(ProviderStreamEvent::ReasoningSignatureDelta(
            "nature".into(),
        ));
        state.push(ProviderStreamEvent::RedactedThinking("opaque".into()));
        state.push(ProviderStreamEvent::MessageEnd {
            stop_reason: Some(StopReason::EndTurn),
        });
        let result = state.finish();
        assert!(
            matches!(&result.response.assistant_message.content[0],MessageContent::Thinking(thinking) if thinking.signature.as_deref()==Some("signature"))
        );
        assert!(
            matches!(&result.response.assistant_message.content[1],MessageContent::RedactedThinking(value) if value=="opaque")
        );
    }
}
