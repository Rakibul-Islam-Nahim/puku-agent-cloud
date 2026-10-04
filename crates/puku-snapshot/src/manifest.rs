//! Manifest types and store.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotStatus {
    /// Manifest durable in Postgres; memory bytes exist ONLY in the workerd
    /// RAM buffer. Visibility-only. NEVER restorable.
    Pending,
    /// Compressed memory file fsynced on the origin host's NVMe, sha256 recorded.
    /// Restorable on the origin host only (local tier, F1/F2).
    LocalDurable,
    /// A verified copy exists off the origin host (RADOS hot pool, or R2 when
    /// the hot pool is off). Restorable anywhere.
    Durable,
    /// Sha256 mismatch on background verify. Never restorable.
    Corrupt,
}

impl SnapshotStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::LocalDurable => "LocalDurable",
            Self::Durable => "Durable",
            Self::Corrupt => "Corrupt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Pending" => Some(Self::Pending),
            "LocalDurable" => Some(Self::LocalDurable),
            "Durable" => Some(Self::Durable),
            "Corrupt" => Some(Self::Corrupt),
            _ => None,
        }
    }

    /// Is this status acceptable for a remote (cross-host) restore?
    /// Only `Durable` qualifies. (RSD §4.4.3 table.)
    pub fn acceptable_for_remote_restore(&self) -> bool {
        matches!(self, Self::Durable)
    }

    /// Is this status acceptable for a local (same-host) restore?
    /// `Pending` is not; `LocalDurable` and `Durable` are. (RSD §4.4.3.)
    pub fn acceptable_for_local_restore(&self) -> bool {
        matches!(self, Self::LocalDurable | Self::Durable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub disk_snap_id: String, // "rbd-sessions/<id>@<ts>"
    pub mem_snap_ref: String, // R2 / RADOS key
    pub mem_snap_size_bytes: u64,
    pub is_full: bool, // true = base (no parent); false = diff
    pub parent_manifest_id: Option<Uuid>, // diff chain link
    pub ts: DateTime<Utc>,
    pub origin_host_id: Uuid,
    pub cpu_flags: Vec<String>,
    pub hypervisor: String,
    pub status: SnapshotStatus,
    pub sha256: String,        // of all of the above
    pub r2_sha256: Option<String>, // set when Durable
}

impl Manifest {
    /// A new pending manifest. `is_full` requires `parent_manifest_id = None`,
    /// a diff requires a parent. Enforced by the schema CHECK
    /// `parent_requires_diff` (RSD §3.3).
    pub fn new_pending(
        session_id: Uuid,
        disk_snap_id: impl Into<String>,
        mem_snap_ref: impl Into<String>,
        mem_snap_size_bytes: u64,
        is_full: bool,
        parent: Option<Uuid>,
        origin_host_id: Uuid,
        cpu_flags: Vec<String>,
        hypervisor: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            disk_snap_id: disk_snap_id.into(),
            mem_snap_ref: mem_snap_ref.into(),
            mem_snap_size_bytes,
            is_full,
            parent_manifest_id: parent,
            ts: Utc::now(),
            origin_host_id,
            cpu_flags,
            hypervisor: hypervisor.into(),
            status: SnapshotStatus::Pending,
            sha256: sha256.into(),
            r2_sha256: None,
        }
    }

    /// Can this manifest be the base of a new chain?
    pub fn is_chain_base(&self) -> bool {
        self.is_full
    }
}

#[async_trait]
pub trait ManifestStore: Send + Sync {
    async fn insert(&self, m: &Manifest) -> Result<(), String>;
    async fn update(&self, m: &Manifest) -> Result<(), String>;
    async fn get(&self, id: Uuid) -> Result<Option<Manifest>, String>;
    async fn latest_durable(&self, session_id: Uuid) -> Result<Option<Manifest>, String>;
    async fn walk_chain(&self, from: Uuid) -> Result<Vec<Manifest>, String>;
    async fn list_for_session(&self, session_id: Uuid) -> Result<Vec<Manifest>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trip() {
        for s in [
            SnapshotStatus::Pending,
            SnapshotStatus::LocalDurable,
            SnapshotStatus::Durable,
            SnapshotStatus::Corrupt,
        ] {
            assert_eq!(SnapshotStatus::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn restore_eligibility() {
        assert!(!SnapshotStatus::Pending.acceptable_for_local_restore());
        assert!(!SnapshotStatus::Pending.acceptable_for_remote_restore());
        assert!(SnapshotStatus::LocalDurable.acceptable_for_local_restore());
        assert!(!SnapshotStatus::LocalDurable.acceptable_for_remote_restore());
        assert!(SnapshotStatus::Durable.acceptable_for_local_restore());
        assert!(SnapshotStatus::Durable.acceptable_for_remote_restore());
        assert!(!SnapshotStatus::Corrupt.acceptable_for_local_restore());
        assert!(!SnapshotStatus::Corrupt.acceptable_for_remote_restore());
    }
}