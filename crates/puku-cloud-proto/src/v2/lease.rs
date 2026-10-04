//! v2 lease messages.
//!
//! Worker-side heartbeat frames. Per RSD §4.2.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseHeartbeat {
    pub host_id: Uuid,
    pub generation: u64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseLostReport {
    pub host_id: Uuid,
    pub reason: LeaseLossReason,
    /// Last known generation, so controld can correlate.
    pub last_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseLossReason {
    ControldUnreachable,
    LeaseExpired,
    ReadOnlyMode,
    SelfShutdown,
    OperatorRequest,
}