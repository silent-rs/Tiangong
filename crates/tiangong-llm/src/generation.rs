//! 输入适配在调用方实现；网络等待前准备独立请求，不借用会话。
use crate::{ModelRequest, ModelResponse, ModelStreamChunk, SingleProviderClient};
use anyhow::Result;

pub trait IntoModelInput {
    fn into_model_input(self, options: &ModelRequest) -> Result<ModelRequest>;
}
impl IntoModelInput for ModelRequest {
    fn into_model_input(self, _options: &ModelRequest) -> Result<ModelRequest> {
        Ok(self)
    }
}

/// 无会话的轻量生成输入；提示词策略由调用方决定。
pub struct TextRequest<'a> {
    pub instruction: &'a str,
    pub input: &'a str,
}
impl IntoModelInput for TextRequest<'_> {
    fn into_model_input(self, options: &ModelRequest) -> Result<ModelRequest> {
        Ok(ModelRequest {
            system: self.instruction.to_owned(),
            messages: vec![crate::message::ChatMessage::text(
                crate::message::MessageRole::User,
                self.input,
            )],
            ..options.clone()
        })
    }
}

/// 一条流的一次消费，包含共享组装与可选实时观察。
pub struct Generation {
    client: SingleProviderClient,
    request: ModelRequest,
}
impl SingleProviderClient {
    /// 同步完成输入准备；返回值不借用 Session，适合直接交给 tokio::spawn。
    pub fn stream<I: IntoModelInput>(
        &self,
        input: I,
        options: &ModelRequest,
    ) -> Result<Generation> {
        Ok(Generation {
            client: self.clone(),
            request: input.into_model_input(options)?,
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
            self.client.stream_async(self.request, chunks).await?,
        )?)
    }
    pub async fn collect<O>(self) -> Result<O>
    where
        O: TryFrom<ModelResponse>,
        O::Error: std::error::Error + Send + Sync + 'static,
    {
        // 不创建无人读取的无界通道；直接使用同一 collector。
        Ok(O::try_from(self.client.collect_async(self.request).await?)?)
    }
    pub async fn collect_text(self) -> Result<String> {
        let response = self.client.collect_async(self.request).await?;
        if response.status != crate::collect::CompletionStatus::Complete
            || !response.invalid_tool_calls.is_empty()
            || !response.tool_calls.is_empty()
        {
            anyhow::bail!("纯文本生成未完整结束或包含工具调用");
        }
        Ok(response.text)
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    #[test]
    fn prepared_generation_does_not_borrow_input_and_options_survive() {
        let client = SingleProviderClient::new(crate::ModelEndpoint::default());
        let generation = {
            let text = String::from("标题输入");
            client
                .stream(
                    TextRequest {
                        instruction: "标题",
                        input: &text,
                    },
                    &ModelRequest {
                        max_output_tokens: Some(12),
                        temperature: Some(0.3),
                        ..Default::default()
                    },
                )
                .unwrap()
        };
        assert_eq!(generation.request.max_output_tokens, Some(12));
        assert_eq!(generation.request.temperature, Some(0.3));
        assert_eq!(generation.request.messages.len(), 1);
        assert!(generation.request.tools.is_empty());
    }
    #[tokio::test]
    async fn failed_input_conversion_does_not_start_network() {
        struct InvalidInput;
        impl IntoModelInput for InvalidInput {
            fn into_model_input(self, _: &ModelRequest) -> Result<ModelRequest> {
                anyhow::bail!("输入无法转换")
            }
        }
        let client = SingleProviderClient::new(crate::ModelEndpoint::default());
        assert!(
            client
                .stream(InvalidInput, &ModelRequest::default())
                .is_err()
        );
    }
}
