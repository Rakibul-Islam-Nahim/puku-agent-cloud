//! Scheduled jobs: cron entries that create ordinary sessions at fire time.
//!
//! The fire loop runs on every controld instance; `FOR UPDATE SKIP LOCKED`
//! makes firing safe with multiple instances (each due row is claimed by
//! exactly one). A schedule that is over quota still advances its
//! `next_run_at` — a missed window is skipped, not queued.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

use crate::{db, AppState};

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct ScheduleRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub name: String,
    pub prompt: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    /// The same policy an interactive session can carry. A scheduled run
    /// used to get none of this, which made cron both less capable and
    /// less constrainable than the equivalent `puku cloud run`.
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub permission_mode: Option<String>,
    pub max_turns: Option<i32>,
    pub connectors: bool,
    pub packs: Vec<String>,
    pub idle_timeout_s: Option<i32>,
    pub cron: String,
    pub enabled: bool,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_session_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// Excluded from memory once, on the schedule, rather than per firing.
    pub memory_opt_out: bool,
    /// Engine every run of this schedule boots under.
    pub engine: String,
}

const SCHEDULE_COLS: &str = "id, org_id, user_id, name, prompt, repo, branch, model, \
    max_budget_usd::float8 AS max_budget_usd, allowed_tools, disallowed_tools, \
    permission_mode, max_turns, connectors, packs, idle_timeout_s, \
    cron, enabled, next_run_at, last_run_at, last_session_id, created_at, \
     memory_opt_out, engine";

/// Parse a five-field cron expression (UTC) and return the next fire time.
pub fn next_fire(cron_expr: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let fields = cron_expr.split_whitespace().count();
    if fields != 5 {
        anyhow::bail!("cron must have 5 fields (minute hour day month weekday), got {fields}");
    }
    // The cron crate wants a seconds field; pin it to 0.
    let schedule = cron::Schedule::from_str(&format!("0 {cron_expr}"))
        .with_context(|| format!("invalid cron expression {cron_expr:?}"))?;
    schedule
        .after(&after)
        .next()
        .context("cron expression never fires")
}

pub struct NewSchedule {
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub name: String,
    pub prompt: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub permission_mode: Option<String>,
    pub max_turns: Option<i32>,
    pub connectors: Option<bool>,
    pub packs: Vec<String>,
    pub idle_timeout_s: Option<i32>,
    pub cron: String,
    /// Excluded from memory for every run this schedule fires.
    pub memory_opt_out: bool,
    /// Already resolved against the deployment's allowed set.
    pub engine: puku_cloud_proto::Engine,
}

pub async fn create(pool: &sqlx::PgPool, new: NewSchedule) -> Result<ScheduleRow> {
    let next = next_fire(&new.cron, Utc::now())?;
    let row = sqlx::query_as::<_, ScheduleRow>(&format!(
        "INSERT INTO schedules (id, org_id, user_id, name, prompt, repo, branch, model, \
         max_budget_usd, allowed_tools, disallowed_tools, permission_mode, max_turns, \
         connectors, packs, idle_timeout_s, cron, next_run_at, memory_opt_out, engine) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9::float8::numeric,$10,$11,$12,$13, \
                 COALESCE($14, true),$15,$16,$17,$18,$19,$20) \
         RETURNING {SCHEDULE_COLS}"
    ))
    .bind(Uuid::new_v4())
    .bind(new.org_id)
    .bind(new.user_id)
    .bind(&new.name)
    .bind(&new.prompt)
    .bind(&new.repo)
    .bind(&new.branch)
    .bind(&new.model)
    .bind(new.max_budget_usd)
    .bind(&new.allowed_tools)
    .bind(&new.disallowed_tools)
    .bind(&new.permission_mode)
    .bind(new.max_turns)
    .bind(new.connectors)
    .bind(&new.packs)
    .bind(new.idle_timeout_s)
    .bind(&new.cron)
    .bind(next)
    .bind(new.memory_opt_out)
    .bind(new.engine.as_str())
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn list(pool: &sqlx::PgPool, org_id: Uuid) -> Result<Vec<ScheduleRow>> {
    Ok(sqlx::query_as::<_, ScheduleRow>(&format!(
        "SELECT {SCHEDULE_COLS} FROM schedules WHERE org_id = $1 ORDER BY created_at DESC"
    ))
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<ScheduleRow>> {
    Ok(sqlx::query_as::<_, ScheduleRow>(&format!(
        "SELECT {SCHEDULE_COLS} FROM schedules WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn set_enabled(pool: &sqlx::PgPool, id: Uuid, enabled: bool) -> Result<()> {
    // Re-anchor next_run_at on enable so a long-disabled schedule doesn't
    // fire immediately for a window that passed months ago.
    let row = get(pool, id).await?.context("schedule not found")?;
    let next = next_fire(&row.cron, Utc::now())?;
    sqlx::query("UPDATE schedules SET enabled = $2, next_run_at = $3 WHERE id = $1")
        .bind(id)
        .bind(enabled)
        .bind(next)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn remove(pool: &sqlx::PgPool, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM schedules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fire one schedule now: create the session and stamp the schedule.
/// Returns the created session id.
pub async fn fire(state: &AppState, sched: &ScheduleRow) -> Result<Uuid> {
    if let Err(reason) = db::check_quota(&state.pool, sched.org_id).await? {
        anyhow::bail!("over quota: {reason}");
    }
    let session = db::create_session(
        &state.pool,
        db::NewSession {
            memory_opt_out: sched.memory_opt_out,
            org_id: sched.org_id,
            user_id: sched.user_id,
            // A scheduled run is recognised by its schedule, not its prompt.
            title: Some(if sched.name.trim().is_empty() {
                db::title_from_prompt(&sched.prompt)
            } else {
                sched.name.clone()
            }),
            prompt: sched.prompt.clone(),
            repo: sched.repo.clone(),
            branch: sched.branch.clone(),
            model: sched.model.clone(),
            max_budget_usd: sched.max_budget_usd,
            // The schedule's own policy. `build_spec` still clamps
            // permission_mode to the deployment ceiling, so this widens
            // what an operator can configure, never what a caller can grant
            // themselves.
            allowed_tools: sched.allowed_tools.clone(),
            disallowed_tools: sched.disallowed_tools.clone(),
            permission_mode: sched.permission_mode.clone(),
            max_turns: sched.max_turns,
            // Unattended: there is no live caller to borrow a bearer from,
            // so the org's stored credential (or the operator's global one)
            // is resolved at dispatch instead.
            // Unattended: no live caller to borrow a bearer from, so
            // `resolve_credential` falls through to the org's stored
            // credential (POST /v1/credentials) and then to the operator's.
            credential: None,
            connectors: Some(sched.connectors),
            // Empty falls through to the org's default packs, so a
            // schedule that names none behaves exactly as it did before
            // `packs` existed.
            packs: sched.packs.clone(),
            // Schedules do not carry a schema yet; per-schedule support can
            // follow the same path packs took.
            output_schema: None,
            // Scheduled runs are unattended: park the VM soon after the
            // agent goes quiet instead of holding it the full default.
            idle_timeout_s: Some(sched.idle_timeout_s.unwrap_or(180)),
            max_duration_s: None,
            // Not a teleport: no transcript, and the id is generated.
            id: None,
            puku_session_id: None,
            import_ref: None,
            import_inline: None,
            // Resolved when the schedule was created. Not re-checked against
            // the allowed set here: narrowing PUKU_ENGINES_ALLOWED gates new
            // requests, it does not quietly switch a standing job to another
            // hypervisor. The dispatcher still only places it where a worker
            // runs that engine.
            engine: puku_cloud_proto::Engine::parse(&sched.engine)
                .unwrap_or(puku_cloud_proto::Engine::Unsupported),
        },
    )
    .await?;
    sqlx::query("UPDATE schedules SET last_run_at = now(), last_session_id = $2 WHERE id = $1")
        .bind(sched.id)
        .bind(session.id)
        .execute(&state.pool)
        .await?;
    db::audit(
        &state.pool,
        Some(sched.org_id),
        sched.user_id,
        "schedule.fire",
        &sched.id.to_string(),
        serde_json::json!({"session_id": session.id, "name": sched.name}),
    )
    .await
    .ok();
    Ok(session.id)
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick(&state).await {
                tracing::warn!(error = format!("{e:#}"), "scheduler tick failed");
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    });
}

/// Fingerprint-aware placement.
///
/// Schedules don't carry a fingerprint today, but the dispatcher that
/// ultimately picks a worker does. This function returns the right column
/// expression to use in worker-selection so a scheduled run lands on the
/// same host its sibling sessions land on (RSD §4.3.3).
#[allow(dead_code)] // Designed in docs/RELIABILITY-REBUILD.md but not wired in yet (PLAN.md section 4).
pub fn fingerprint_expr() -> &'static str {
    "COALESCE((SELECT host_id FROM sessions WHERE fingerprint = $1 AND host_id IS NOT NULL \
               ORDER BY created_at DESC LIMIT 1), $2::uuid)"
}

/// Decide which host a new session should prefer, given its fingerprint.
/// Returns the host id of the most-recent sibling session with the same
/// fingerprint, or `None` if there is none. The dispatcher uses this to
/// avoid paying RBD-snap cost on a fresh host (RSD §4.3.3).
#[allow(dead_code)] // Designed in docs/RELIABILITY-REBUILD.md but not wired in yet (PLAN.md section 4).
pub async fn preferred_host_for_fingerprint(
    pool: &sqlx::PgPool,
    fingerprint: &str,
) -> Result<Option<Uuid>> {
    let row: Option<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT host_id FROM sessions WHERE fingerprint = $1 AND host_id IS NOT NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fingerprint)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| r.0))
}

async fn tick(state: &AppState) -> Result<()> {
    // Claim due schedules; SKIP LOCKED keeps multi-instance firing exact.
    let mut tx = state.pool.begin().await?;
    let due: Vec<ScheduleRow> = sqlx::query_as(&format!(
        "SELECT {SCHEDULE_COLS} FROM schedules \
         WHERE enabled AND next_run_at <= now() \
         ORDER BY next_run_at LIMIT 10 FOR UPDATE SKIP LOCKED"
    ))
    .fetch_all(&mut *tx)
    .await?;
    // Advance next_run_at inside the claim transaction so a crash between
    // advance and fire skips a window rather than double-firing.
    for sched in &due {
        let next = next_fire(&sched.cron, Utc::now())?;
        sqlx::query("UPDATE schedules SET next_run_at = $2 WHERE id = $1")
            .bind(sched.id)
            .bind(next)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    for sched in &due {
        match fire(state, sched).await {
            Ok(session_id) => {
                tracing::info!(schedule = %sched.id, %session_id, name = %sched.name, "schedule fired");
            }
            Err(e) => {
                tracing::warn!(schedule = %sched.id, error = format!("{e:#}"), "schedule fire failed");
            }
        }
    }
    if !due.is_empty() {
        crate::api::dispatch_pending(state).await?;
    }
    Ok(())
}
