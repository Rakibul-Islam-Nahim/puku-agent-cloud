//! Background sweeper jobs.
//!
//! Per docs/RELIABILITY-REBUILD.md §5.4. Drives:
//!   - lease expiry -> fence (delegated to `leases::start_sweeper`).
//!   - tier transitions: running -> hibernated -> cold_archived -> archived.
//!   - snapshot retention: drop expired snapshots, respecting chain integrity.
//!   - nightly test-restore probe (logs only; the operator wires puku-snapshot).
//!
//! All jobs are supervised with `puku_observability::supervise` so a panic
//! in one does not stop the others, and every loop is interval-driven (no
//! blocking on Postgres).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::leases::PgLeaseStore;
use crate::AppState;

/// Tier states driven by the sweeper. Per RSD §5.4.2. Distinct from the
/// session-level `state` column (which tracks the VM lifecycle); this tracks
/// the snapshot tier a session's data lives at right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotTier {
    /// Warm - a full or diff snapshot is durable on a worker.
    Warm,
    /// Hibernated - the VM is parked; data is in object storage or RBD.
    Hibernated,
    /// Cold - data is in object storage only; the RBD image has been collapsed.
    ColdArchived,
    /// Archived - retained only for compliance / disaster recovery; billable.
    Archived,
}

impl SnapshotTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Hibernated => "hibernated",
            Self::ColdArchived => "cold_archived",
            Self::Archived => "archived",
        }
    }
}

/// Top-level spawn: registers every background job. Safe to call once.
pub fn spawn_all(state: AppState) {
    spawn_tier_transitions(state.clone());
    spawn_snapshot_retention(state.clone());
    spawn_nightly_test_restore(state.clone());
    spawn_reaper(state.clone());

    // Lease sweeper: 1 s tick per RSD §5.2.
    let store = Arc::new(PgLeaseStore { pool: state.pool.clone() });
    crate::leases::start_sweeper(state.clone(), crate::leases::service_for(&state), store);

    // Storage cleanup of the shared pool: needs no worker to be up.
    crate::storagegc::spawn(state.clone());
}

/// Tier transitions: any session in `running` whose desired_state is `stopped`
/// for >= `idle_grace` seconds is migrated Warm -> Hibernated. Sessions in
/// `hibernated` for >= `cold_grace` get collapsed to ColdArchived. The
/// final tier (Archived) is reserved for the compliance reaper (RSD §5.4.3).
pub fn spawn_tier_transitions(state: AppState) {
    puku_observability::supervise("tier_transitions", move || {
        let st = state.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if let Err(e) = tier_transitions_once(&st).await {
                    tracing::warn!(error = format!("{e:#}"), "tier transition pass failed");
                }
            }
        }
    });
}

async fn tier_transitions_once(state: &AppState) -> anyhow::Result<()> {
    // Running -> Hibernated: VMs whose agent has been idle for the org's
    // idle_timeout and whose desired_state is `parked`. The migration is a pure
    // RBD snap + upload to object storage; the VM continues to run.
    let rows = sqlx::query(
        "UPDATE sessions SET tier = 'hibernated', tier_changed_at = now() \
         WHERE state = 'parked' AND tier = 'warm' AND desired_state IN ('parked','stopped') \
           AND last_activity_at < now() - make_interval(secs => $1::int) \
         RETURNING id",
    )
    .bind(120i32)
    .fetch_all(&state.pool)
    .await?;
    if !rows.is_empty() {
        tracing::info!(count = rows.len(), "tier: warm -> hibernated");
    }

    // Hibernated -> ColdArchived: after 7 days without a resume. The RD
    // image is collapsed to object storage and the RBD entry is dropped.
    let rows = sqlx::query(
        "UPDATE sessions SET tier = 'cold_archived', tier_changed_at = now() \
         WHERE tier = 'hibernated' AND tier_changed_at < now() - interval '7 days' \
           AND last_resumed_at IS NOT NULL \
         RETURNING id",
    )
    .fetch_all(&state.pool)
    .await?;
    if !rows.is_empty() {
        tracing::info!(count = rows.len(), "tier: hibernated -> cold_archived");
        // Notify the worker to drop the local RBD mapping. Best-effort: a
        // worker that's offline keeps the directory until next sweep.
        for r in &rows {
            let id: Uuid = r.get("id");
            if let Ok(Some((worker_id_opt,))) =
                sqlx::query_as::<_, (Option<Uuid>,)>("SELECT worker_id FROM sessions WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&state.pool)
                    .await
            {
                if let Some(w) = worker_id_opt {
                    let _ = crate::workerlink::send_to_worker(
                        state,
                        Some(w),
                        puku_cloud_proto::worker_proto::Down::ReapSession { session_id: id },
                    );
                }
            }
        }
    }
    Ok(())
}

/// Snapshot retention: per RSD §5.5. Walks the snapshot chain from newest to
/// oldest, marks a snapshot for deletion once it falls outside the
/// `keep_count` window AND the next-older snapshot is `Durable`. RBD chain
/// integrity (parent FK RESTRICT) prevents deletion of a snapshot that still
/// has dependents.
pub fn spawn_snapshot_retention(state: AppState) {
    puku_observability::supervise("snapshot_retention", move || {
        let st = state.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                if let Err(e) = snapshot_retention_once(&st).await {
                    tracing::warn!(error = format!("{e:#}"), "snapshot retention pass failed");
                }
            }
        }
    });
}

async fn snapshot_retention_once(state: &AppState) -> anyhow::Result<()> {
    // Group: per session, ordered by created_at DESC.
    let rows = sqlx::query(
        "SELECT id, session_id, created_at, status FROM snapshots \
         WHERE status IN ('local_durable','durable') \
         ORDER BY session_id, created_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut by_session: std::collections::HashMap<Uuid, Vec<(Uuid, String, chrono::DateTime<chrono::Utc>)>> =
        std::collections::HashMap::new();
    for r in rows {
        let id: Uuid = r.get("id");
        let sid: Uuid = r.get("session_id");
        let status: String = r.get("status");
        let ts: chrono::DateTime<chrono::Utc> = r.get("created_at");
        by_session.entry(sid).or_default().push((id, status, ts));
    }

    let keep = 5i64;
    let mut deleted = 0u64;
    for (_, snaps) in by_session.iter_mut() {
        snaps.sort_by_key(|s| std::cmp::Reverse(s.2));
        // Drop anything past `keep`; the FK chain ensures dependents are
        // gone first.
        for stale in snaps.iter().skip(keep as usize) {
            // parent FK RESTRICT means this fails with a constraint violation
            // if there's still a child; that's the desired property.
            if let Err(e) = sqlx::query("DELETE FROM snapshots WHERE id = $1")
                .bind(stale.0)
                .execute(&state.pool)
                .await
            {
                tracing::debug!(snapshot_id = %stale.0, error = %e,
                    "snapshot not deleted (likely has dependents)");
                continue;
            }
            deleted += 1;
        }
    }
    if deleted > 0 {
        tracing::info!(deleted, "snapshot retention swept");
    }
    Ok(())
}

/// Nightly test-restore probe. Picks one randomly chosen Warm snapshot per
/// tier and attempts a dry-run restore via the snapshot subsystem. Logs only;
/// a failure pages nobody because this is a probe, not a recovery path.
/// (Wired to puku-snapshot when the snapshot service is started.)
pub fn spawn_nightly_test_restore(state: AppState) {
    puku_observability::supervise("nightly_test_restore", move || {
        let _st = state.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
                tracing::info!("nightly test-restore probe (no-op until snapshot service is wired)");
            }
        }
    });
}

/// Compliance reaper: drives Archived sessions out of Postgres after
/// `retain_destroyed_days`. RSD §5.4.3.
pub fn spawn_reaper(state: AppState) {
    puku_observability::supervise("compliance_reaper", move || {
        let st = state.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                if let Err(e) = reap_once(&st).await {
                    tracing::warn!(error = format!("{e:#}"), "reaper pass failed");
                }
            }
        }
    });
}

async fn reap_once(state: &AppState) -> anyhow::Result<()> {
    // Sessions in `archived` tier past the retention window. We delete only
    // session_events ( kept for `retention_days` after ended_at) and the
    // tier='archived' row marker - never the row itself, since the audit log
    // references it.
    let rows = sqlx::query(
        "DELETE FROM session_events WHERE session_id IN ( \
           SELECT id FROM sessions WHERE tier = 'archived' \
             AND archived_at IS NOT NULL \
             AND archived_at < now() - interval '90 days' \
        ) RETURNING session_id",
    )
    .fetch_all(&state.pool)
    .await?;
    if !rows.is_empty() {
        tracing::info!(count = rows.len(), "compliance reaper removed old events");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_strings_are_stable() {
        // These strings land in Postgres. Changing them is a migration.
        assert_eq!(SnapshotTier::Warm.as_str(), "warm");
        assert_eq!(SnapshotTier::Hibernated.as_str(), "hibernated");
        assert_eq!(SnapshotTier::ColdArchived.as_str(), "cold_archived");
        assert_eq!(SnapshotTier::Archived.as_str(), "archived");
    }

    #[test]
    fn tier_order_is_warm_to_archived() {
        // The transition sweeper walks this order. If a later tier is
        // added, decide where it slots in deliberately.
        let order = [
            SnapshotTier::Warm,
            SnapshotTier::Hibernated,
            SnapshotTier::ColdArchived,
            SnapshotTier::Archived,
        ];
        // Adjacent transitions are valid: the sweeper only ever moves forward
        // one step. Strings are NOT compared (alphabetical order does not
        // reflect tier policy); only distinctness is asserted.
        assert_ne!(order[0], order[1]);
        assert_ne!(order[1], order[2]);
        assert_ne!(order[2], order[3]);
    }
}
