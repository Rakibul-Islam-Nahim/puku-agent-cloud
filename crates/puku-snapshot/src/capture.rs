//! Capture buffer: Phase 1 memory + the helper for Phase 2 fsync.

use sha2::{Digest, Sha256};

/// The bytes the VM is paused for. They are the same bytes Phase 3
/// compresses and uploads, and the manifest refers to. (RSD §4.4.2.)
#[derive(Debug, Clone)]
pub struct CaptureBuffer {
    pub session_id: uuid::Uuid,
    pub ts_unix: i64,
    pub bytes: Vec<u8>,
}

impl CaptureBuffer {
    pub fn new(session_id: uuid::Uuid, ts_unix: i64, bytes: Vec<u8>) -> Self {
        Self {
            session_id,
            ts_unix,
            bytes,
        }
    }

    /// SHA-256 of the buffer contents. This is what the manifest records.
    pub fn sha256_hex(&self) -> String {
        let mut h = Sha256::new();
        h.update(&self.bytes);
        hex::encode(h.finalize())
    }

    /// Compressed size estimate (real compressors vary; we report bytes
    /// here, callers can replace with zstd).
    pub fn size_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// Decision: should the trigger fire a snapshot now?
pub fn should_capture(
    last_snapshot_unix: Option<i64>,
    now_unix: i64,
    tier_rpo: Option<u32>,
    effective_mode_is_warm: bool,
) -> bool {
    if !effective_mode_is_warm {
        return false;
    }
    let Some(interval) = tier_rpo else {
        return false;
    };
    match last_snapshot_unix {
        Some(last) => (now_unix - last) >= interval as i64,
        None => true, // never taken, capture now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_stable() {
        let b = CaptureBuffer::new(uuid::Uuid::new_v4(), 0, b"hello".to_vec());
        // sha256("hello") = 2cf24d...
        let h = b.sha256_hex();
        assert_eq!(&h[..8], "2cf24dba");
    }

    #[test]
    fn trigger_respects_rpo() {
        assert!(!should_capture(Some(0), 4, Some(5), true)); // 4 < 5
        assert!(should_capture(Some(0), 5, Some(5), true)); // 5 >= 5
        assert!(should_capture(Some(0), 100, Some(60), true));
    }

    #[test]
    fn trigger_skips_cold() {
        assert!(!should_capture(None, 100, Some(5), false));
    }

    #[test]
    fn trigger_cold_only_tier() {
        assert!(!should_capture(Some(0), 100, None, true));
    }
}