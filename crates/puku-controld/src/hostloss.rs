//! What happens when the lease sweeper declares a worker host dead.
//!
//! Until now a host that died and never came back left everything it ran
//! looking alive: machines stayed `running`, sessions stayed `running` or
//! `waiting_input`, and nothing but the host's return would ever change
//! that. This module settles every one of them, each the only way its data
//! allows:
//!
//! - **Machines** with a ready snapshot are restored on another worker at
//!   once (the caller's copy of the volume is the snapshot). Without one
//!   they are stopped, saying why; a start then waits for the host until the
//!   volume grace runs out, as it always has. A machine that was stopping
//!   is simply stopped.
//! - **Sessions** keep their workspace on that host's disk and nowhere
//!   else, so they cannot move. A running one is stopped -- resumable, and a
//!   resume goes back to the host if it returns, or fails with a reason once
//!   it has been gone past the grace period. One that was still booting is
//!   failed (resumable too). One the host was handed but never started goes
//!   back in the queue.
//!
//! Fencing. Sessions on shared disks (`sharedvol`) are fenced when they
//! are next placed, not here: the fence goes with the move, and a session
//! nobody resumes never needs one. Host-local session volumes and machine
//! volumes cannot be attached by any other host: nothing to cut off.
//! What can still double-run is a machine restored here while the old host
//! was only partitioned; the old host's copy is stopped by the reconnect
//! reconciliation when it comes back (the machine row names another worker
//! now). Once volumes move to RBD, a storage fence (`puku-fence`) goes in
//! front of all of this and no restore runs until it has succeeded.

use uuid::Uuid;

use puku_cloud_proto::session::SessionState;

use crate::db::{self, machines as mdb, snapshots as sdb};
use crate::AppState;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct HostLossReport {
    pub machines_restored: Vec<Uuid>,
    pub machines_stopped: Vec<Uuid>,
    pub sessions_stopped: Vec<Uuid>,
    pub sessions_failed: Vec<Uuid>,
    pub sessions_requeued: Vec<Uuid>,
    /// Set when the host turned out to be connected here after all, and
    /// nothing was touched.
    pub skipped_connected: bool,
}

/// Settle everything a dead host was running. Idempotent: a second call
/// finds nothing left on the host.
pub async fn on_host_dead(state: &AppState, host: Uuid) -> anyhow::Result<HostLossReport> {
    let mut report = HostLossReport::default();
    if state.workers.get(host).is_some() {
        // Its link to this instance is up: the declaration raced a renewal
        // stall. The link drops on the failed renewal and the worker
        // re-registers; nothing here should move.
        tracing::warn!(host_id = %host, "host declared dead but still linked here; leaving it alone");
        report.skipped_connected = true;
        return Ok(report);
    }

    settle_machines(state, host, &mut report).await?;
    settle_sessions(state, host, &mut report).await?;

    db::audit(
        &state.pool,
        None,
        None,
        "host.declared_dead",
        &host.to_string(),
        serde_json::json!({
            "machines_restored": report.machines_restored,
            "machines_stopped": report.machines_stopped,
            "sessions_stopped": report.sessions_stopped,
            "sessions_failed": report.sessions_failed,
            "sessions_requeued": report.sessions_requeued,
            "fence": "on move: shared-disk sessions are fenced when next placed",
        }),
    )
    .await
    .ok();
    tracing::warn!(host_id = %host, ?report, "settled a dead host's machines and sessions");

    if !report.machines_restored.is_empty() {
        crate::api::machines::dispatch_machines(state).await?;
    }
    if !report.sessions_requeued.is_empty() {
        crate::api::dispatch_pending(state).await?;
    }
    Ok(report)
}

async fn settle_machines(state: &AppState, host: Uuid, report: &mut HostLossReport) -> anyhow::Result<()> {
    // Handed over but never booted: back in the queue as they are.
    mdb::release_unbooted(&state.pool, host, &[]).await?;

    for (id, was) in mdb::stop_on_dead_host(&state.pool, host).await? {
        mdb::close_runs(&state.pool, id).await?;
        let wanted_running = was != "stopping";
        let latest = if wanted_running && crate::snapshots::enabled(state) {
            sdb::latest_ready(&state.pool, id).await?
        } else {
            None
        };
        let restored = match (latest, mdb::get(&state.pool, id).await?) {
            (Some(s), Some(m)) => mdb::schedule_restore(&state.pool, id, s.id, None, m.cpus, m.memory_mib)
                .await?
                .map(|_| s.id),
            _ => None,
        };
        match restored {
            Some(snapshot) => {
                tracing::warn!(machine = %id, host_id = %host, %snapshot, "restoring a dead host's machine elsewhere");
                report.machines_restored.push(id);
            }
            None => report.machines_stopped.push(id),
        }
    }
    Ok(())
}

async fn settle_sessions(state: &AppState, host: Uuid, report: &mut HostLossReport) -> anyhow::Result<()> {
    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT id, state, volume_shared FROM sessions WHERE worker_id = $1 \
           AND state IN ('scheduled','booting','bootstrapping','running','waiting_input','stopping')",
    )
    .bind(host)
    .fetch_all(&state.pool)
    .await?;

    for (id, st, shared) in rows {
        let Some(cur) = SessionState::parse(&st) else { continue };
        let (next, reason) = match cur {
            SessionState::Scheduled => {
                // Never started there. Unassign and let the dispatcher place
                // it: a fresh session goes anywhere, a resume waits for (or
                // fails on) its volume host as any resume does.
                let moved = sqlx::query(
                    "UPDATE sessions SET worker_id = NULL WHERE id = $1 AND worker_id = $2 AND state = 'scheduled'",
                )
                .bind(id)
                .bind(host)
                .execute(&state.pool)
                .await?
                .rows_affected();
                if moved > 0 {
                    report.sessions_requeued.push(id);
                }
                continue;
            }
            SessionState::Booting | SessionState::Bootstrapping => (
                SessionState::Failed,
                "the worker host was lost while this session was booting; resume to try again",
            ),
            _ if shared => (
                SessionState::Stopped,
                "the worker host running this session was lost; its disk is on shared storage, \
                 so a resume continues on another host once the old one is fenced",
            ),
            _ => (
                SessionState::Stopped,
                "the worker host running this session was lost; its workspace is on that host, \
                 so a resume waits for the host to return",
            ),
        };
        match db::transition(&state.pool, id, next, Some(reason)).await {
            Ok((_, ev)) => {
                if let Some(ev) = ev {
                    state.publish_events(&[ev]).await;
                }
                if next == SessionState::Failed {
                    report.sessions_failed.push(id);
                } else {
                    report.sessions_stopped.push(id);
                }
            }
            // Moved on meanwhile (the user stopped it, a late report): fine.
            Err(e) => tracing::info!(session = %id, error = %e, "dead-host session already moved on"),
        }
    }
    Ok(())
}
