use crate::WorldEvent;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Keep the full observer stream while sessions receive presence through their
/// atomic peer snapshot/continuation only. Movement must not fill their second
/// queue and evict block, chat or private presentation updates.
#[derive(Clone)]
pub(crate) struct Events {
    all: broadcast::Sender<Arc<WorldEvent>>,
    sessions: broadcast::Sender<Arc<WorldEvent>>,
}

impl Events {
    pub fn new(capacity: usize) -> Self {
        Self {
            all: broadcast::channel(capacity).0,
            sessions: broadcast::channel(capacity).0,
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<WorldEvent>> {
        self.all.subscribe()
    }
    pub fn subscribe_sessions(&self) -> broadcast::Receiver<Arc<WorldEvent>> {
        self.sessions.subscribe()
    }
    pub fn send(
        &self,
        event: Arc<WorldEvent>,
    ) -> Result<usize, broadcast::error::SendError<Arc<WorldEvent>>> {
        if !matches!(
            event.as_ref(),
            WorldEvent::Joined(_)
                | WorldEvent::Moved { .. }
                | WorldEvent::Left { .. }
                | WorldEvent::Equipped(_)
        ) {
            let _ = self.sessions.send(event.clone());
        }
        self.all.send(event)
    }
}
