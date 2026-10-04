//! Audit sink for fence actions. Writes to `fence_log` in Postgres.

use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub host_id: Uuid,
    pub session_id: Option<Uuid>,
    pub action: String,
    pub outcome: String,
    pub detail: serde_json::Value,
    pub requested_by: String,
}

#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Returns the audit log id assigned by the sink.
    async fn write(&self, entry: &AuditEntry) -> i64;
}

/// In-memory audit sink for tests.
pub struct InMemoryAuditSink {
    pub entries: tokio::sync::Mutex<Vec<AuditEntry>>,
}

impl InMemoryAuditSink {
    pub fn new() -> Self {
        Self {
            entries: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Default for InMemoryAuditSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AuditSink for InMemoryAuditSink {
    async fn write(&self, entry: &AuditEntry) -> i64 {
        let mut g = self.entries.lock().await;
        g.push(entry.clone());
        g.len() as i64
    }
}