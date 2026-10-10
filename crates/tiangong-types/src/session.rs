//! 会话类型

use serde::{Deserialize, Serialize};

use crate::message::{Message, MessageRole, now_text};

/// 会话
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    #[serde(deserialize_with = "crate::message::deserialize_messages")]
    pub messages: Vec<Message>,
    pub created_at: String,
    pub updated_at: String,
}

impl Session {
    pub fn new(title: &str) -> Self {
        let now = now_text();
        Self {
            id: scru128::new().to_string(),
            title: title.to_string(),
            messages: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    pub fn append_message(&mut self, role: MessageRole, content: impl Into<String>) {
        self.messages.push(Message::new(role, content));
        self.updated_at = now_text();
    }

    pub fn append_message_with_reasoning(
        &mut self,
        role: MessageRole,
        content: impl Into<String>,
        reasoning: impl Into<String>,
    ) {
        self.messages
            .push(Message::with_reasoning(role, content, reasoning));
        self.updated_at = now_text();
    }
}
