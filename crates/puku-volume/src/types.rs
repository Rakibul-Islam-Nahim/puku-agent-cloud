//! Shared types: identifiers and a simple newtype for a device path.

use std::path::PathBuf;
use uuid::Uuid;

/// Volume identifier. Convention: `rbd-sessions/<uuid>` or `local/<uuid>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VolumeId(pub String);

impl VolumeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VolumeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host identifier (== `workers.id` in Postgres).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostId(pub Uuid);

impl HostId {
    pub fn new(uuid: Uuid) -> Self {
        Self(uuid)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Snapshot identifier: which volume and which RBD snapshot name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapId {
    pub volume: VolumeId,
    pub name: String, // "<uuid>@<unix_ts>"
}

impl SnapId {
    pub fn new(volume: VolumeId, name: impl Into<String>) -> Self {
        Self {
            volume,
            name: name.into(),
        }
    }
}

/// The path a volume is mapped to on a host (an `/dev/rbd<N>` for RBD,
/// a bind mount for Local). Backend-agnostic; consumers do not need to
/// know what kind of path it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePath(pub PathBuf);

impl DevicePath {
    pub fn new(p: impl Into<PathBuf>) -> Self {
        Self(p.into())
    }
    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}