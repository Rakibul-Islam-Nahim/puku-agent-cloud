//! Off-cluster disk backup (RSD §4.7).
//!
//! Ceph (3 replicas) protects against disk and host loss; this protects
//! against losing the pool itself. The procedure:
//!
//! 1. `rbd snap create` on the volume (no VM pause; guest has been `sync`ed).
//! 2. If the backup chain is empty: `rbd export` the snapshot.
//!    Otherwise: `rbd export-diff --from-snap <last_to_snap> <to_snap>`.
//! 3. Skip if the diff is empty (idle sessions produce no writes).
//! 4. Mark `Durable` after the R2 read-back sha256 matches.
//! 5. Keep the previous `to_snap` as the next `from_snap`; delete older RBD
//!    backup snapshots.
//!
//! Compaction: 24 diffs -> new full, retire the old chain (children first).

use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    Full,
    Diff,
}

#[derive(Debug, Clone)]
pub struct BackupRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub kind: BackupKind,
    pub from_snap: Option<String>,
    pub to_snap: String,
    pub r2_key: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct BackupChain {
    pub records: Vec<BackupRecord>,
}

impl BackupChain {
    pub fn last_to_snap(&self) -> Option<&str> {
        self.records.last().map(|r| r.to_snap.as_str())
    }

    pub fn diff_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| r.kind == BackupKind::Diff)
            .count()
    }

    /// Should we take a new full? Yes when the diff chain reaches the cap.
    pub fn needs_full(&self, diff_cap: usize) -> bool {
        self.diff_count() >= diff_cap
    }
}

/// Decide what to do for a session's next backup tick.
pub fn plan_next_backup(chain: &BackupChain, diff_cap: usize) -> NextBackup {
    if chain.records.is_empty() {
        return NextBackup::Full;
    }
    if chain.needs_full(diff_cap) {
        return NextBackup::Full;
    }
    NextBackup::Diff {
        from: chain.last_to_snap().unwrap().to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextBackup {
    Full,
    Diff { from: String },
}

pub const DEFAULT_DIFF_CAP: usize = 24;

/// A pluggable sink for the bytes. Production wires R2; tests use a stub.
pub trait BackupSink: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String>;
    fn get(&self, key: &str) -> Result<Vec<u8>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: BackupKind, from: Option<&str>, to: &str) -> BackupRecord {
        BackupRecord {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            kind,
            from_snap: from.map(String::from),
            to_snap: to.into(),
            r2_key: "k".into(),
            size_bytes: 1,
            sha256: "h".into(),
        }
    }

    #[test]
    fn empty_chain_takes_full() {
        let c = BackupChain { records: vec![] };
        assert_eq!(plan_next_backup(&c, 24), NextBackup::Full);
    }

    #[test]
    fn single_full_then_diff() {
        let c = BackupChain {
            records: vec![rec(BackupKind::Full, None, "snap1")],
        };
        assert_eq!(
            plan_next_backup(&c, 24),
            NextBackup::Diff {
                from: "snap1".to_string()
            }
        );
    }

    #[test]
    fn diff_cap_triggers_new_full() {
        let mut records = vec![rec(BackupKind::Full, None, "snap1")];
        for i in 0..24 {
            records.push(rec(BackupKind::Diff, Some("snap1"), &format!("d{}", i)));
        }
        let c = BackupChain { records };
        assert_eq!(plan_next_backup(&c, 24), NextBackup::Full);
    }

    #[test]
    fn needs_full_threshold() {
        let mut records = vec![rec(BackupKind::Full, None, "snap1")];
        for i in 0..23 {
            records.push(rec(BackupKind::Diff, Some("snap1"), &format!("d{}", i)));
        }
        let c = BackupChain { records };
        assert!(!c.needs_full(24));
    }

    /// Compile-only: BackupSink is a trait object and tests can swap impls.
    #[allow(dead_code)]
    fn _check_sink_object_safe(_sink: &dyn BackupSink) {}
    #[allow(dead_code)]
    fn _check_path_object_safe(_p: std::path::PathBuf) {}
}