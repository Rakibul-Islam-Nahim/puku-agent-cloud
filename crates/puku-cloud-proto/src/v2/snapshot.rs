//! v2 snapshot messages.
//!
//! Per RSD §4.5. Frames for snapshot creation, manifest handshake, and
//! retention pings.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRequest {
    pub session_id: Uuid,
    pub host_id: Uuid,
    pub trigger: SnapshotTrigger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotTrigger {
    Periodic,
    PreRecovery,
    OnIdle,
    OnTierTransition,
    Operator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub is_full: bool,
    pub status: String, // matches puku-snapshot::SnapshotStatus::as_str
    pub size_bytes: u64,
    pub sha256: String,
    pub ts_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip() {
        let m = SnapshotManifest {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            parent_id: Some(Uuid::new_v4()),
            is_full: false,
            status: "durable".into(),
            size_bytes: 1<<20,
            sha256: "deadbeef".into(),
            ts_ms: 1,
        };
        let s = serde_json::to_string(&m).unwrap();
        let p: SnapshotManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(p.id, m.id);
        assert_eq!(p.parent_id, m.parent_id);
        assert_eq!(p.is_full, m.is_full);
    }
}