//! Event stream types.

use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub session_id: Uuid,
    pub seq: u64,
    pub kind: String,    // "agent" | "session" | "exec" | "user" | "system"
    pub payload: serde_json::Value,
}

/// A sink the proxy writes events into. Production: the WebSocket.
/// Tests: an in-memory recorder.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn write(&self, ev: &Event) -> Result<(), String>;
}

pub struct InMemoryEventSink {
    pub events: tokio::sync::Mutex<Vec<Event>>,
}

impl InMemoryEventSink {
    pub fn new() -> Self {
        Self {
            events: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Default for InMemoryEventSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventSink for InMemoryEventSink {
    async fn write(&self, ev: &Event) -> Result<(), String> {
        self.events.lock().await.push(ev.clone());
        Ok(())
    }
}

/// Source of past events. Production: Postgres `session_events`.
/// The contract is "give me events for this session with seq > N".
#[async_trait]
pub trait EventSource: Send + Sync {
    async fn events_after(&self, session_id: Uuid, after_seq: u64) -> Result<Vec<Event>, String>;
}