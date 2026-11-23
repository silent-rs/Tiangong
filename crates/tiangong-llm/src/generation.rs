//! 配置好的 client 只接收已准备的消息；轻量映射与请求装配在 LLM 内完成。
use crate::request::ProviderRequest;
use crate::{ModelResponse, ModelStreamChunk, SingleProviderClient};
use anyhow::Result;
use tiangong_types::Message;

/// 一次逻辑生成拥有的请求与配置快照，不借用 Session 或调用方消息。
pub struct Generation {
    client: SingleProviderClient,
    request: ProviderRequest,
}
impl SingleProviderClient {
    pub fn stream(&self, messages: Vec<Message>) -> Result<Generation> {
        let request = self.request_from_messages(messages)?;
        Ok(Generation {
            client: self.clone(),
            request,
        })
    }
}
impl Generation {
    pub async fn collect_with<O>(
        self,
        chunks: tokio::sync::mpsc::UnboundedSender<ModelStreamChunk>,
    ) -> Result<O>
    where
        O: TryFrom<ModelResponse>,
        O::Error: std::error::Error + Send + Sync + 'static,
    {
        Ok(O::try_from(
            self.client
                .consume_request(self.request, Some(chunks))
                .await?,
        )?)
    }
    pub async fn collect<O>(self) -> Result<O>
    where
        O: TryFrom<ModelResponse>,
        O::Error: std::error::Error + Send + Sync + 'static,
    {
        Ok(O::try_from(
            self.client.consume_request(self.request, None).await?,
        )?)
    }
    pub async fn collect_text(self) -> Result<String> {
        let response: ModelResponse = self.collect().await?;
        if response.status != crate::CompletionStatus::Complete
            || !response.invalid_tool_calls.is_empty()
            || !response.tool_calls.is_empty()
        {
            anyhow::bail!("纯文本生成未完整结束或包含工具调用");
        }
        Ok(response.text)
    }
    pub async fn collect_text_with_usage(self) -> Result<(String, Option<crate::TokenUsageData>)> {
        let response: ModelResponse = self.collect().await?;
        if response.status != crate::CompletionStatus::Complete
            || !response.invalid_tool_calls.is_empty()
            || !response.tool_calls.is_empty()
        {
            anyhow::bail!("纯文本生成未完整结束或包含工具调用");
        }
        Ok((response.text, response.provider_usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelEndpoint, Options, ReasoningEffort, tool::ToolSpec};
    use tiangong_types::{ContentBlock, MediaKind, MessageRole, StoredAsset};

    fn messages() -> Vec<Message> {
        vec![
            Message::new(MessageRole::System, "fixed"),
            Message::new(MessageRole::User, "hello"),
        ]
    }
    fn client() -> SingleProviderClient {
        SingleProviderClient::new(ModelEndpoint {
            model: "model".into(),
            ..Default::default()
        })
    }

    #[test]
    fn options_are_captured_once_and_derived_client_does_not_mutate_original() {
        let base = client().with_options(Options {
            max_output_tokens: 100,
            temperature: Some(0.3),
            cache_key: Some("session-a".into()),
            tools: vec![ToolSpec {
                name: "read".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }],
            ..Default::default()
        });
        let before = base.stream(messages()).unwrap();
        let mut options = base.options().clone();
        options.max_output_tokens = 20;
        options.tools.clear();
        options.reasoning_effort = ReasoningEffort::None;
        options.cache_key = Some("session-b".into());
        let derived = base.clone().with_options(options);
        let after = derived.stream(messages()).unwrap();
        assert_eq!(before.request.max_tokens, 100);
        assert_eq!(base.options().max_output_tokens, 100);
        assert_eq!(after.request.max_tokens, 20);
        assert_eq!(before.request.tools.len(), 1);
        assert!(after.request.tools.is_empty());
        assert_eq!(before.request.session_id.as_deref(), Some("session-a"));
        assert_eq!(after.request.session_id.as_deref(), Some("session-b"));
    }
    #[test]
    fn invalid_input_fails_before_any_network_and_no_system_is_required_for_plain_prompt() {
        assert!(client().stream(Vec::new()).is_err());
        assert!(
            client()
                .stream(vec![Message::new(MessageRole::Notice, "notice")])
                .is_err()
        );
        assert!(
            client()
                .stream(vec![Message::new(MessageRole::User, "hello")])
                .is_ok()
        );
    }
    #[test]
    fn local_image_is_not_read_by_llm_even_when_file_exists() {
        let path = std::env::temp_dir().join(format!("llm-no-read-{}.png", scru128::new()));
        std::fs::write(&path, [1u8, 2, 3]).unwrap();
        let mut message = Message::new(MessageRole::User, "image");
        message.content.push(ContentBlock::Image {
            asset: StoredAsset {
                asset_id: "image".into(),
                local_path: path.to_string_lossy().into(),
                original_name: "image.png".into(),
                mime_type: "image/png".into(),
                size: 3,
                kind: MediaKind::Image,
            },
            data: None,
        });
        let error = client()
            .stream(vec![message.clone()])
            .err()
            .expect("LLM 不可读取本地文件");
        assert!(error.to_string().contains("图片尚未准备"));
        if let ContentBlock::Image { data, .. } = &mut message.content[1] {
            *data = Some("AQID".into());
        }
        let generation = client().stream(vec![message]).unwrap();
        assert!(
            matches!(&generation.request.messages[0].content[1],crate::message::MessageContent::Image(image) if image.data=="data:image/png;base64,AQID")
        );
        std::fs::remove_file(path).unwrap();
    }
}
