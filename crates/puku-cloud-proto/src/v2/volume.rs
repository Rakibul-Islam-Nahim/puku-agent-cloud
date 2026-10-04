//! v2 volume messages.
//!
//! Per RSD §4.1. Wire types for volume attach/detach/relocate. The actual
//! Ceph shell-outs happen in puku-volume; these frames carry the envelope.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeCreate {
    pub session_id: Uuid,
    pub host_id: Uuid,
    pub size_bytes: u64,
    pub tier: String, // warm|hibernated|cold_archived|archived
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeAttach {
    pub session_id: Uuid,
    pub host_id: Uuid,
    /// The fence receipt proving it is safe to attach on a new host.
    pub fence_receipt_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRelocate {
    pub session_id: Uuid,
    pub from_host: Uuid,
    pub to_host: Uuid,
    pub fence_receipt_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRef {
    pub session_id: Uuid,
    pub host_id: Uuid,
    pub volume_id: String,
    pub tier: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_relocate_carries_fence_receipt() {
        let m = VolumeRelocate {
            session_id: Uuid::new_v4(),
            from_host: Uuid::new_v4(),
            to_host: Uuid::new_v4(),
            fence_receipt_id: Some(42),
        };
        let s = serde_json::to_string(&m).unwrap();
        let p: VolumeRelocate = serde_json::from_str(&s).unwrap();
        assert_eq!(p.fence_receipt_id, Some(42));
        assert_ne!(p.from_host, p.to_host);
    }
}