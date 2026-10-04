//! Fence trait.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use puku_leases::BmcEndpoint;

#[derive(Debug, thiserror::Error)]
pub enum FenceError {
    #[error("blocklist failed: {0}")]
    Blocklist(String),
    #[error("bmc unreachable: {0}")]
    BmcUnreachable(String),
    #[error("bmc power action failed: {0}")]
    BmcAction(String),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Clone)]
pub struct FenceReceipt {
    pub host_id: Uuid,
    pub blocklisted_at: DateTime<Utc>,
    pub bmc_action: Option<String>, // "power_off"|"power_cycle"|"none"
    pub audit_log_id: i64,
    /// Volumes cut off at the storage layer, and the client addresses that
    /// were blocklisted for them. Empty for a host-only fence.
    pub volumes: Vec<String>,
    pub clients: Vec<String>,
}

#[async_trait]
pub trait Fence: Send + Sync {
    /// Block the host from writing ANY RBD volume.
    /// Returns Ok only after blocklist is confirmed.
    async fn blocklist(&self, host_id: Uuid) -> Result<(), FenceError>;

    /// Block + optional BMC power cycle.
    /// Returns Ok only after both succeed (or BMC is not configured).
    /// The BMC failure must NOT fail the whole fence: blocklist is the
    /// primary fence. The receipt records what happened.
    async fn fence(
        &self,
        host_id: Uuid,
        bmc: Option<&BmcEndpoint>,
    ) -> Result<FenceReceipt, FenceError>;

    /// Reverse blocklist (post-recovery).
    async fn unfence(&self, host_id: Uuid) -> Result<(), FenceError>;

    /// The fence recovery must pass before it attaches `volumes` anywhere
    /// else: the host is fenced, then every client that has each volume open
    /// is cut off at the storage layer. `Ok` only when every volume is fenced;
    /// one failure fails the whole call, so nothing is attached half-fenced.
    async fn fence_volumes(
        &self,
        host_id: Uuid,
        session_id: Option<Uuid>,
        volumes: &[puku_volume::VolumeId],
        bmc: Option<&BmcEndpoint>,
    ) -> Result<FenceReceipt, FenceError>;
}