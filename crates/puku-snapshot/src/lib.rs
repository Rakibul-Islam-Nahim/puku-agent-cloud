//! Snapshot subsystem.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.4. This is one cohesive design:
//! trigger, capture, durability, retention, restore. They are tightly
//! coupled -- a mistake in one breaks the others.
//!
//! Status model (RSD §4.4.1):
//!
//! ```text
//! Pending       -> visibility-only, NEVER restorable
//! LocalDurable  -> fsynced on origin NVMe, local restore only
//! Durable       -> verified off-host copy (RADOS hot pool, or R2 when off)
//! Corrupt       -> sha256 mismatch, never restorable
//! ```

pub mod capture;
pub mod chain;
pub mod compaction;
pub mod disk_backup;
pub mod idle_signal;
pub mod manifest;
pub mod retention;
pub mod restore;
pub mod test_restore;
pub mod triggers;

pub use capture::CaptureBuffer;
pub use chain::{ChainState, group_into_chains, walk_to_base};
pub use compaction::{CompactionPlan, CompactionTrigger, OverlayMerger, Page, overlay_merge, should_compact, MERGE_DEPTH};
pub use disk_backup::{BackupChain, BackupKind, BackupRecord, NextBackup, plan_next_backup, DEFAULT_DIFF_CAP};
pub use idle_signal::IdleSignal;
pub use manifest::{Manifest, ManifestStore, SnapshotStatus};
pub use retention::{is_valid_reaper_order, reapable_newest_first};
pub use restore::{RestoreBackend, RestoreError, RestoreOutcome, RestoreService};
pub use test_restore::{InMemoryObjectStore, ObjectStore, TestRestoreJob, VerifyResult};
pub use triggers::{
    effective_recovery_mode, rpo_seconds, RecoveryMode, SlaTier, TriggerDecision,
};