//! A VM that died or hung while its host stayed up (the worker watchdog's
//! verdict): restart it in place, unless it keeps crashing.
//!
//! A session that was mid-turn is stopped and resumed at once on its own
//! disk (the same host, which is connected) with a message saying what
//! happened. One that was waiting for an answer is only stopped: the answer
//! resumes it. A machine is started again. Either way, a third crash within
//! `CRASH_WINDOW` stops the automatic restarts and says so: something in
//! the guest is killing it, and restarting for ever only hides that.

use puku_cloud_proto::session::SessionState;
use uuid::Uuid;

use crate::db::{self, machines as mdb};
use crate::AppState;

/// Crashes counted for the crash-loop guard.
const CRASH_WINDOW_MINUTES: i32 = 30;
/// The crash that stops automatic restarts.
pub const MAX_CRASHES: i64 = 3;

/// What a session restarted after its VM crashed is told.
pub const CRASH_PROMPT: &str = "The virtual machine running this session crashed in the middle of your task, \
and it was restarted with your files intact. Continue the task from where you left off. Check the current \
state of the workspace first: the last few seconds of work before the crash may not have been saved.";

/// Record a crash and return how many there were in the window, this one
/// included.
async fn count_crash(state: &AppState, action: &str, id: Uuid, detail: &str) -> anyhow::Result<i64> {
    db::audit(&state.pool, None, None, action, &id.to_string(), serde_json::json!({"detail": detail})).await?;
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = $1 AND subject = $2 \
           AND ts > now() - make_interval(mins => $3)",
    )
    .bind(action)
    .bind(id.to_string())
    .bind(CRASH_WINDOW_MINUTES)
    .fetch_one(&state.pool)
    .await?)
}

#[derive(Debug, PartialEq, Eq)]
pub enum CrashOutcome {
    Restarted,
    Stopped,
    GaveUp,
}

pub async fn on_session_crash(state: &AppState, session_id: Uuid, detail: &str) -> anyhow::Result<CrashOutcome> {
    let Some(session) = db::get_session(&state.pool, session_id).await? else { return Ok(CrashOutcome::Stopped) };
    let was = session.session_state();
    let crashes = count_crash(state, "session.vm_crashed", session_id, detail).await?;
    let next = match was {
        SessionState::Booting | SessionState::Bootstrapping => SessionState::Failed,
        _ => SessionState::Stopped,
    };
    let gave_up = crashes >= MAX_CRASHES;
    let reason = if gave_up {
        format!("the VM crashed {crashes} times in {CRASH_WINDOW_MINUTES} minutes; not restarting it automatically (last: {detail})")
    } else {
        format!("the VM crashed: {detail}")
    };
    match db::transition(&state.pool, session_id, next, Some(&reason)).await {
        Ok((_, Some(ev))) => state.publish_events(&[ev]).await,
        Ok((_, None)) => {}
        Err(e) => {
            tracing::info!(session = %session_id, error = %e, "crashed session already moved on");
            return Ok(CrashOutcome::Stopped);
        }
    }
    if gave_up {
        tracing::error!(session = %session_id, crashes, "session VM keeps crashing; automatic restarts stopped");
        return Ok(CrashOutcome::GaveUp);
    }
    if was != SessionState::Running {
        return Ok(CrashOutcome::Stopped);
    }
    crate::hostloss::resume_elsewhere(state, session_id, CRASH_PROMPT).await?;
    crate::api::dispatch_pending(state).await?;
    tracing::warn!(session = %session_id, crashes, "session VM crashed; restarted on its own disk");
    Ok(CrashOutcome::Restarted)
}

pub async fn on_machine_crash(state: &AppState, machine_id: Uuid, detail: &str) -> anyhow::Result<CrashOutcome> {
    let crashes = count_crash(state, "machine.vm_crashed", machine_id, detail).await?;
    if crashes >= MAX_CRASHES {
        let msg = format!(
            "the VM crashed {crashes} times in {CRASH_WINDOW_MINUTES} minutes; not restarting it automatically (last: {detail})"
        );
        mdb::note_refusal(&state.pool, machine_id, "crash_loop", &msg).await?;
        tracing::error!(machine = %machine_id, crashes, "machine VM keeps crashing; automatic restarts stopped");
        return Ok(CrashOutcome::GaveUp);
    }
    if mdb::schedule_start(&state.pool, machine_id).await?.is_none() {
        return Ok(CrashOutcome::Stopped);
    }
    crate::api::machines::dispatch_machines(state).await?;
    tracing::warn!(machine = %machine_id, crashes, "machine VM crashed; restarted");
    Ok(CrashOutcome::Restarted)
}
