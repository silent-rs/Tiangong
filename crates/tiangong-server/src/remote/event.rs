use tiangong_types::{IncomingMessage, OutgoingMessage};
use tokio::sync::broadcast;

/// Server 侧远程入口事件总线
#[derive(Debug, Clone)]
pub enum TiangongEvent {
    MessageReceived(Box<IncomingMessage>),
    MessageSent {
        session_id: String,
        message: OutgoingMessage,
    },
    SessionCreated(String),
    TurnCompleted {
        session_id: String,
        success: bool,
    },
    ConfigChanged,
    Shutdown,
}

pub struct EventBus {
    sender: broadcast::Sender<TiangongEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn publish(&self, event: TiangongEvent) {
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TiangongEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}
