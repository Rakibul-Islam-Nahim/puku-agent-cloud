//! Volume backend traits.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.1.2.

use std::path::PathBuf;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::VolumeError;
use crate::types::{DevicePath, HostId, SnapId, VolumeId};

/// What a volume backend can do.
#[async_trait]
pub trait VolumeBackend: Send + Sync {
    /// Create a new session volume as CoW clone of `base`.
    async fn create(
        &self,
        session_id: Uuid,
        base: &SnapId,
        on_host: HostId,
    ) -> Result<VolumeId, VolumeError>;

    /// Map the volume onto `host`. Idempotent if already mapped.
    async fn attach(&self, vol: &VolumeId, host: HostId)
        -> Result<DevicePath, VolumeError>;

    /// Unmap from `host`. Idempotent.
    async fn detach(&self, vol: &VolumeId, host: HostId) -> Result<(), VolumeError>;

    /// Take a crash-consistent snapshot of the volume.
    /// The disk must be flushed before this call (guest issues `sync` over vsock).
    async fn snapshot(&self, vol: &VolumeId, on_host: HostId) -> Result<SnapId, VolumeError>;

    /// Move a volume's mapping from `from` to `to`.
    /// Backend may implement as detach+attach or live migration.
    async fn relocate(
        &self,
        vol: &VolumeId,
        from: HostId,
        to: HostId,
    ) -> Result<DevicePath, VolumeError>;

    /// Block a host from writing. MUST be called before any failover that writes
    /// the same volume; otherwise split-brain.
    async fn fence(&self, host: HostId) -> Result<(), VolumeError>;

    /// Unblock a host (post-recovery).
    async fn unfence(&self, host: HostId) -> Result<(), VolumeError>;

    /// Fast health check: does `host` currently have I/O access to `vol`?
    async fn is_reachable(&self, vol: &VolumeId, host: HostId) -> Result<bool, VolumeError>;
}

/// Top-level volume abstraction. Just exposes the backend.
pub trait Volume: Send + Sync {
    fn backend(&self) -> &dyn VolumeBackend;
}

/// Helper: turn a `DevicePath` into the host-side mountpoint the worker
/// writes through. Most backends return a path that is already a mountpoint,
/// but RBD may return a block device that needs mounting first. This trait
/// default does nothing and backends can override.
#[async_trait]
pub trait Mount: Send + Sync {
    async fn ensure_mounted(&self, dev: &DevicePath, host: HostId) -> Result<PathBuf, VolumeError>;
}