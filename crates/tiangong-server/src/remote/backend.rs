use anyhow::Result;
use async_trait::async_trait;
use tiangong_types::{MediaAsset, MessageAnnotations, OutgoingMessage};

use super::core::ServerCoreManager;

/// Server 请求实际使用的 Core 后端类型。
///
/// standalone 模式拥有自己的 [`ServerCoreManager`]；Desktop 内嵌模式必须使用
/// 宿主注入的桥接后端，不能在同一进程内再创建一套 Core 映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreBackendKind {
    Standalone,
    EmbeddedHost,
}

#[async_trait]
pub trait ServerCoreBackend: Send + Sync {
    fn kind(&self) -> CoreBackendKind;

    async fn send_connector_message_and_wait(
        &self,
        connector: &str,
        channel_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
        annotations: MessageAnnotations,
    ) -> Result<(String, OutgoingMessage)>;

    async fn send_message_and_wait(
        &self,
        session_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
        annotations: MessageAnnotations,
    ) -> Result<(String, OutgoingMessage)>;

    async fn delete_session(&self, session_id: &str) -> Result<bool>;

    /// 取消会话当前执行中的轮次（快速停止：等待方立即收到取消错误）。
    /// 返回 true 表示取消命令已投递到活跃执行；false 表示当时没有可接受命令的
    /// 活跃执行，不代表会话资源不存在。
    async fn cancel_session_turn(&self, session_id: &str) -> Result<bool>;

    async fn sync_config_from_state(&self) -> Result<()>;
}

#[async_trait]
impl ServerCoreBackend for ServerCoreManager {
    fn kind(&self) -> CoreBackendKind {
        CoreBackendKind::Standalone
    }

    async fn send_connector_message_and_wait(
        &self,
        connector: &str,
        channel_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
        annotations: MessageAnnotations,
    ) -> Result<(String, OutgoingMessage)> {
        ServerCoreManager::send_connector_message_and_wait(
            self,
            connector,
            channel_id,
            content,
            message_id,
            media,
            annotations,
        )
        .await
    }

    async fn send_message_and_wait(
        &self,
        session_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
        annotations: MessageAnnotations,
    ) -> Result<(String, OutgoingMessage)> {
        ServerCoreManager::send_message_and_wait(
            self,
            session_id,
            content,
            message_id,
            media,
            annotations,
        )
        .await
    }

    async fn delete_session(&self, session_id: &str) -> Result<bool> {
        ServerCoreManager::delete_session(self, session_id).await
    }

    async fn cancel_session_turn(&self, session_id: &str) -> Result<bool> {
        ServerCoreManager::cancel_session_turn(self, session_id).await
    }

    async fn sync_config_from_state(&self) -> Result<()> {
        ServerCoreManager::sync_config_from_state(self).await;
        Ok(())
    }
}
