//! Machine snapshots (docs/MACHINES-API.md, "Snapshots").
//!
//! A snapshot is a machine's disks in object storage, so the machine can be
//! restored -- on any worker, at the size it had -- after the worker that
//! held them is gone. Cold and disk-level: the volume, as a tar of its host
//! directory, and for a machine with `persist_root` its root disk. Never
//! memory, and never the decrypted spec a worker keeps beside the volume.
//!
//! Bucket credentials stay on controld. A worker asks for presigned part URLs
//! over the control link and PUTs each part straight to the bucket.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Feature a worker advertises when it can capture and restore snapshots.
/// controld sends no snapshot frame, and places no restore, anywhere else.
pub const FEATURE_SNAPSHOTS: &str = "snapshots";

/// Every part but the last is exactly this size: R2 refuses a multipart
/// upload whose parts differ.
pub const DEFAULT_PART_BYTES: u64 = 64 << 20;
/// S3's floor for every part but the last.
pub const MIN_PART_BYTES: u64 = 5 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotLayer {
    /// The machine's volume directory.
    Volume,
    /// The writable root disk of a `persist_root` machine: what was installed.
    Root,
}

impl SnapshotLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotLayer::Volume => "volume",
            SnapshotLayer::Root => "root",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "volume" => Some(SnapshotLayer::Volume),
            "root" => Some(SnapshotLayer::Root),
            _ => None,
        }
    }
}

/// Why a snapshot was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTrigger {
    Stop,
    Manual,
    Periodic,
    Destroy,
    Update,
}

impl SnapshotTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotTrigger::Stop => "stop",
            SnapshotTrigger::Manual => "manual",
            SnapshotTrigger::Periodic => "periodic",
            SnapshotTrigger::Destroy => "destroy",
            SnapshotTrigger::Update => "update",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "stop" => SnapshotTrigger::Stop,
            "manual" => SnapshotTrigger::Manual,
            "periodic" => SnapshotTrigger::Periodic,
            "destroy" => SnapshotTrigger::Destroy,
            "update" => SnapshotTrigger::Update,
            _ => return None,
        })
    }

    /// Whether a machine unchanged since its last snapshot may skip this one.
    /// A manual snapshot never does: the caller asked for a snapshot, not for
    /// a check.
    pub fn may_skip(self) -> bool {
        !matches!(self, SnapshotTrigger::Manual)
    }
}

/// How far a snapshot's files can be trusted to agree with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    /// Taken with the VM stopped: exactly what was on disk.
    Clean,
    /// Taken while the VM ran: each file as it was when read, like a backup
    /// of a live system. A database mid-write may need recovery on restore.
    Live,
}

impl Consistency {
    pub fn as_str(self) -> &'static str {
        match self {
            Consistency::Clean => "clean",
            Consistency::Live => "live",
        }
    }
}

/// A capture's progress, as the worker reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotProgress {
    Uploading,
    Ready,
    /// Nothing changed since the last snapshot; nothing was uploaded.
    Skipped,
    Failed,
}

/// One layer of a snapshot and the object it is stored under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerOrder {
    pub layer: SnapshotLayer,
    pub key: String,
    /// sha256 of the stored object, hex. On a restore, checked after the
    /// download; absent on a capture, which is what computes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// What a machine's disks looked like, cheaply: two captures with the same
/// fingerprint would hold the same bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// Capture a machine's disks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotOrder {
    pub snapshot_id: Uuid,
    pub machine_id: Uuid,
    pub trigger: SnapshotTrigger,
    /// The snapshot's 32-byte data key, hex. controld keeps it sealed; the
    /// worker already holds the bytes it protects.
    pub key_hex: String,
    pub part_bytes: u64,
    pub layers: Vec<LayerOrder>,
    /// tar patterns under the volume to leave out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    /// The last ready snapshot's fingerprint. Unchanged disks are not
    /// uploaded again: the whole capture is skipped, or an unchanged root
    /// layer reused. Absent when the trigger may not skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Fingerprint>,
}

/// Lay a snapshot's disks down before a machine boots. Rides in
/// `MachineSpec::restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOrder {
    pub snapshot_id: Uuid,
    pub key_hex: String,
    pub layers: Vec<LayerOrder>,
}

/// A presigned URL for one part of a layer's upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartUrl {
    /// 1-based, as S3 numbers them.
    pub part: u32,
    pub url: String,
}

/// One uploaded part, as the bucket acknowledged it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartDone {
    pub part: u32,
    /// Exactly as the bucket returned it, quotes included: completing the
    /// upload echoes it back.
    pub etag: String,
    pub bytes: u64,
}

/// The object a snapshot layer is stored under.
pub fn object_key(machine_id: Uuid, snapshot_id: Uuid, layer: SnapshotLayer) -> String {
    format!("machines/{machine_id}/snapshots/{snapshot_id}/{}.pks", layer.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_scoped_to_the_machine_and_snapshot() {
        let m = Uuid::nil();
        let s = Uuid::max();
        assert_eq!(
            object_key(m, s, SnapshotLayer::Volume),
            format!("machines/{m}/snapshots/{s}/volume.pks")
        );
    }

    #[test]
    fn names_round_trip() {
        for l in [SnapshotLayer::Volume, SnapshotLayer::Root] {
            assert_eq!(SnapshotLayer::parse(l.as_str()), Some(l));
            assert_eq!(serde_json::to_string(&l).unwrap(), format!("\"{}\"", l.as_str()));
        }
        for t in [
            SnapshotTrigger::Stop,
            SnapshotTrigger::Manual,
            SnapshotTrigger::Periodic,
            SnapshotTrigger::Destroy,
            SnapshotTrigger::Update,
        ] {
            assert_eq!(SnapshotTrigger::parse(t.as_str()), Some(t));
        }
        assert!(!SnapshotTrigger::Manual.may_skip());
        assert!(SnapshotTrigger::Stop.may_skip());
    }

    /// An order from a controld that knows nothing of fingerprints or
    /// excludes still reads.
    #[test]
    fn a_minimal_order_parses() {
        let json = r#"{"snapshot_id":"00000000-0000-0000-0000-000000000001",
            "machine_id":"00000000-0000-0000-0000-000000000002","trigger":"stop",
            "key_hex":"00","part_bytes":67108864,
            "layers":[{"layer":"volume","key":"k"}]}"#;
        let o: SnapshotOrder = serde_json::from_str(json).unwrap();
        assert!(o.excludes.is_empty() && o.previous.is_none());
        assert_eq!(o.layers[0].sha256, None);
    }
}
