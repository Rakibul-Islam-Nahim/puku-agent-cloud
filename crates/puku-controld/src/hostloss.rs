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
//! - **Sessions on a shared disk** that were mid-turn are resumed at once:
//!   stopped, then queued again with a "continue where you left off"
//!   message, as if the user had typed it. The dispatcher fences the dead
//!   host off the disk and places the session on another host. One that was
//!   waiting for an answer is only stopped; the answer resumes it.
//! - **Sessions on a host-local disk** cannot move. A running one is
//!   stopped -- resumable, and a resume goes back to the host if it returns,
//!   or fails with a reason once it has been gone past the grace period.
//! - Either kind still booting is failed (resumable too). One the host was
//!   handed but never started goes back in the queue.
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
    /// Machines on a shared disk, queued to boot on another host (after
    /// the dispatcher fences the dead one).
    pub machines_moved: Vec<Uuid>,
    pub machines_stopped: Vec<Uuid>,
    pub sessions_stopped: Vec<Uuid>,
    pub sessions_failed: Vec<Uuid>,
    pub sessions_requeued: Vec<Uuid>,
    /// Shared-disk sessions that were mid-turn, queued to continue elsewhere.
    pub sessions_resumed: Vec<Uuid>,
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
            "machines_moved": report.machines_moved,
            "machines_stopped": report.machines_stopped,
            "sessions_stopped": report.sessions_stopped,
            "sessions_failed": report.sessions_failed,
            "sessions_requeued": report.sessions_requeued,
            "sessions_resumed": report.sessions_resumed,
            "fence": "on move: shared-disk sessions are fenced when next placed",
        }),
    )
    .await
    .ok();
    tracing::warn!(host_id = %host, ?report, "settled a dead host's machines and sessions");

    if !report.machines_restored.is_empty() || !report.machines_moved.is_empty() {
        crate::api::machines::dispatch_machines(state).await?;
    }
    if !report.sessions_requeued.is_empty() || !report.sessions_resumed.is_empty() {
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
        let row = mdb::get(&state.pool, id).await?;
        if wanted_running && row.as_ref().is_some_and(|m| m.volume_shared) && state.shared_volumes.is_some() {
            // Its disk -- volume and root disk, every installed package --
            // moves with it: no snapshot, no rollback.
            if mdb::schedule_start(&state.pool, id).await?.is_some() {
                tracing::warn!(machine = %id, host_id = %host, "moving a dead host's shared-disk machine");
                report.machines_moved.push(id);
                continue;
            }
        }
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
                 so it continues on another host once the old one is fenced",
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
                } else if shared && cur == SessionState::Running && state.shared_volumes.is_some() {
                    match resume_elsewhere(state, id).await {
                        Ok(()) => report.sessions_resumed.push(id),
                        Err(e) => {
                            tracing::warn!(session = %id, error = format!("{e:#}"), "auto-resume failed; left stopped");
                            report.sessions_stopped.push(id);
                        }
                    }
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

/// What a session resumed after its host died is told. Its disk is intact up
/// to the last flush, but the turn was cut off: the agent should look before
/// it assumes.
pub const CONTINUE_PROMPT: &str = "The machine running this session failed in the middle of your task, and the \
session was moved to another machine with its files intact. Continue the task from where you left off. \
Check the current state of the workspace first: the last few seconds of work before the failure may not \
have been saved.";

/// Queue a stopped shared-disk session again, as a resume would. A session
/// that already has a conversation continues it with `CONTINUE_PROMPT`; one
/// whose agent never got going keeps its original prompt and starts over on
/// the same disk.
async fn resume_elsewhere(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let has_conversation: bool =
        sqlx::query_scalar("SELECT puku_session_id IS NOT NULL FROM sessions WHERE id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await?;
    sqlx::query("UPDATE sessions SET worker_id = NULL WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    let prompt = has_conversation.then_some(CONTINUE_PROMPT);
    let (_, ev) = db::transition_with_prompt(&state.pool, id, SessionState::Scheduled, None, prompt).await?;
    if let Some(ev) = ev {
        state.publish_events(&[ev]).await;
    }
    db::audit(&state.pool, None, None, "session.auto_resume", &id.to_string(), serde_json::json!({"reason": "host_dead"}))
        .await
        .ok();
    Ok(())
}
