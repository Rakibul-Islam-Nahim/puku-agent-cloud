//! All Postgres access. Event-seq allocation and the session state machine
//! live here so every caller gets the same transactional guarantees.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use puku_cloud_proto::event::{Event, EventKind, GuestEvent};
use puku_cloud_proto::session::SessionState;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

pub mod machines;
pub mod snapshots;

/// Column list with numeric->float8 casts so rows map onto plain f64.
const SESSION_COLS: &str = "id, org_id, user_id, worker_id, title, prompt, repo, branch, model, \
     max_budget_usd::float8 AS max_budget_usd, allowed_tools, disallowed_tools, \
     permission_mode, max_turns, credential_kind, connectors, packs, output_schema, \
     branch_pushed, pr_url, imported_from, state, \
     sandbox_name, volume_path, puku_session_id, last_seq, last_guest_line, pending_question, \
     cost_usd::float8 AS cost_usd, tokens_in, tokens_out, cache_read_tokens, \
     cache_write_tokens, idle_timeout_s, max_duration_s, error, \
     created_at, started_at, ended_at, \
     memory_profile_id, memory_preamble, memory_opt_out, memory_ingested_at, \
     engine, volume_worker_id, volume_shared";

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct SessionRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub worker_id: Option<Uuid>,
    pub title: String,
    pub prompt: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// NULL until the caller asks for one; `build_spec` resolves NULL to the
    /// deployment default and clamps anything else to the ceiling.
    pub permission_mode: Option<String>,
    pub max_turns: Option<i32>,
    /// Which credential this session runs on: the caller's own, or None
    /// meaning the operator's global fallback. The ciphertext itself is
    /// deliberately NOT part of this row — it is read only at dispatch, so
    /// it never rides along in a list response or a serialized log line.
    pub credential_kind: Option<String>,
    /// Whether brokered connectors are attached at dispatch.
    pub connectors: bool,
    /// Skill packs requested for this session (`name` or `name@range`).
    pub packs: Vec<String>,
    pub output_schema: Option<serde_json::Value>,
    /// Set when the runner pushes; `pr_url` follows once the PR is open.
    pub branch_pushed: Option<String>,
    pub pr_url: Option<String>,
    /// The local puku-cli session this was teleported from, if any — so a
    /// client can say "continued from your laptop" rather than showing a
    /// resumed session with no visible history.
    pub imported_from: Option<String>,
    pub state: String,
    pub sandbox_name: String,
    pub volume_path: Option<String>,
    pub puku_session_id: Option<String>,
    pub last_seq: i64,
    pub last_guest_line: i64,
    pub pending_question: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub idle_timeout_s: i32,
    pub max_duration_s: i32,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Memory profile this session reads from and writes to. Resolved once,
    /// then pinned, so a resume never drifts onto a different profile.
    pub memory_profile_id: Option<String>,
    /// The preamble as delivered. Stored rather than re-fetched so a parked
    /// session resumes with exactly the system prompt it started with.
    pub memory_preamble: Option<String>,
    pub memory_opt_out: bool,
    /// NULL until this session's transcript reached the memory service.
    /// `archive.rs` reads it to decide whether the transcript may be deleted.
    pub memory_ingested_at: Option<DateTime<Utc>>,
    /// Hypervisor this session boots under, pinned at create. A resume never
    /// changes it: the volumes were laid out for this engine's guest.
    pub engine: String,
    /// The worker holding this session's volumes, once any worker has
    /// reported on it. Resume must go back there -- see migration 0021.
    pub volume_worker_id: Option<Uuid>,
    /// The disk is an RBD image on the shared cluster (migration 0033).
    pub volume_shared: bool,
}

impl SessionRow {
    pub fn session_state(&self) -> SessionState {
        SessionState::parse(&self.state).unwrap_or(SessionState::Failed)
    }

    /// The CHECK constraint keeps this parseable; `Unsupported` is the
    /// honest answer if it ever is not, and the dispatcher refuses it.
    pub fn session_engine(&self) -> puku_cloud_proto::Engine {
        puku_cloud_proto::Engine::parse(&self.engine).unwrap_or(puku_cloud_proto::Engine::Unsupported)
    }
}

pub struct NewSession {
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub title: Option<String>,
    pub prompt: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub permission_mode: Option<String>,
    pub max_turns: Option<i32>,
    /// Encrypted caller credential + its kind, or None to fall back to the
    /// operator's global one.
    pub credential: Option<(String, Vec<u8>)>,
    pub connectors: Option<bool>,
    pub packs: Vec<String>,
    /// JSON Schema the final answer must satisfy; None for prose runs.
    pub output_schema: Option<serde_json::Value>,
    pub idle_timeout_s: Option<i32>,
    pub max_duration_s: Option<i32>,
    /// Keep this session out of memory entirely, in both directions.
    pub memory_opt_out: bool,
    /// Pre-chosen id, for a caller that must derive an object key before the
    /// row exists. None generates one, which is the ordinary path.
    pub id: Option<Uuid>,
    /// Teleport fields, written in the INSERT rather than an UPDATE after it.
    ///
    /// A row lands in `created`, which makes it dispatchable *immediately*.
    /// Writing these afterwards left a window in which the dispatcher could
    /// send a spec with `resume: false` and no transcript -- the imported
    /// conversation silently gone, which is the "pushed session does not
    /// remember anything" symptom in the troubleshooting table. Nothing
    /// marked the row as still-under-construction, so the only fix is to
    /// construct it in one statement.
    pub puku_session_id: Option<String>,
    pub import_ref: Option<String>,
    pub import_inline: Option<String>,
    /// Already resolved against the deployment's allowed set by the caller.
    pub engine: puku_cloud_proto::Engine,
}

/// A one-line label for list views. `sessions.title` used to default to ''
/// and nothing ever wrote it, so every session rendered as a bare UUID.
/// Cut on a word boundary so the label reads as a phrase, not a truncation.
pub fn title_from_prompt(prompt: &str) -> String {
    const MAX: usize = 60;
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    // Take MAX chars, then back off to the last space so we don't cut a word
    // in half. A single 60-char word has no space to back off to: keep it.
    let head: String = flat.chars().take(MAX).collect();
    let cut = match head.rfind(' ') {
        Some(i) if i >= MAX / 2 => &head[..i],
        _ => head.as_str(),
    };
    format!("{}…", cut.trim_end())
}

pub async fn create_session(pool: &PgPool, new: NewSession) -> Result<SessionRow> {
    let id = new.id.unwrap_or_else(Uuid::new_v4);
    let sandbox_name = format!("ses-{}", &id.simple().to_string()[..12]);
    let title = new
        .title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| title_from_prompt(&new.prompt));
    let row = sqlx::query_as::<_, SessionRow>(&format!(
        "INSERT INTO sessions (id, org_id, user_id, title, prompt, repo, branch, model, \
         max_budget_usd, allowed_tools, disallowed_tools, permission_mode, max_turns, \
         credential_kind, credential_enc, connectors, packs, output_schema, sandbox_name, \
         idle_timeout_s, max_duration_s, puku_session_id, imported_from, import_ref, \
         import_inline, memory_opt_out, engine) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9::float8::numeric,$10,$11,$12,$13,$14,$15, \
                 COALESCE($16, true),$17,$18,$19, \
                 COALESCE($20, 900), COALESCE($21, 14400),$22,$22,$23,$24,$25,$26) \
         RETURNING {SESSION_COLS}"
    ))
    .bind(id)
    .bind(new.org_id)
    .bind(new.user_id)
    .bind(&title)
    .bind(&new.prompt)
    .bind(&new.repo)
    .bind(&new.branch)
    .bind(&new.model)
    .bind(new.max_budget_usd)
    .bind(&new.allowed_tools)
    .bind(&new.disallowed_tools)
    .bind(&new.permission_mode)
    .bind(new.max_turns)
    .bind(new.credential.as_ref().map(|(kind, _)| kind.clone()))
    .bind(new.credential.as_ref().map(|(_, enc)| enc.clone()))
    .bind(new.connectors)
    .bind(&new.packs)
    .bind(&new.output_schema)
    .bind(&sandbox_name)
    .bind(new.idle_timeout_s)
    .bind(new.max_duration_s)
    .bind(&new.puku_session_id)
    .bind(&new.import_ref)
    .bind(&new.import_inline)
    .bind(new.memory_opt_out)
    .bind(new.engine.as_str())
    .fetch_one(pool)
    .await
    .context("inserting session")?;
    Ok(row)
}

/// Record which worker holds a session's volumes, the first time any worker
/// reports on it. Never overwritten: a stray frame from another worker must
/// not move the pin away from the host that actually has the disk.
/// `shared`: the worker keeps session disks on the shared Ceph cluster.
pub async fn note_volume_worker(pool: &PgPool, session_id: Uuid, worker_id: Uuid, shared: bool) -> Result<()> {
    sqlx::query(
        "UPDATE sessions SET volume_worker_id = $2, volume_shared = $3          WHERE id = $1 AND volume_worker_id IS NULL",
    )
    .bind(session_id)
    .bind(worker_id)
    .bind(shared)
    .execute(pool)
    .await?;
    Ok(())
}

/// A shared-disk session was placed on another host (after fencing the old
/// one): that host is where its disk is open now. Only ever for shared
/// disks, which is the one case where the disk really can move.
pub async fn move_shared_volume(pool: &PgPool, session_id: Uuid, worker_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE sessions SET volume_worker_id = $2 WHERE id = $1 AND volume_shared")
        .bind(session_id)
        .bind(worker_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A worker's name, status and last heartbeat, for explaining why a session
/// pinned to it cannot be placed.
pub async fn worker_presence(
    pool: &PgPool,
    worker_id: Uuid,
) -> Result<Option<(String, String, Option<DateTime<Utc>>)>> {
    Ok(sqlx::query_as("SELECT name, status, last_heartbeat_at FROM workers WHERE id = $1")
        .bind(worker_id)
        .fetch_optional(pool)
        .await?)
}

/// Read the encrypted credential for a session. Separate from `SessionRow`
/// on purpose: the ciphertext is needed at exactly one place (dispatch) and
/// should not be reachable from anywhere that serializes a session.
pub async fn session_credential(pool: &PgPool, id: Uuid) -> Result<Option<(String, Vec<u8>)>> {
    let row: Option<(Option<String>, Option<Vec<u8>>)> =
        sqlx::query_as("SELECT credential_kind, credential_enc FROM sessions WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        Some((Some(kind), Some(enc))) => Some((kind, enc)),
        _ => None,
    })
}

/// Replace a session's stored credential. Called on resume: a bearer that
/// was valid when the session was created has very likely expired by the
/// time someone resumes it days later, and the resuming request carries a
/// fresh one.
pub async fn set_session_credential(
    pool: &PgPool,
    id: Uuid,
    credential: Option<(String, Vec<u8>)>,
) -> Result<()> {
    sqlx::query("UPDATE sessions SET credential_kind = $2, credential_enc = $3 WHERE id = $1")
        .bind(id)
        .bind(credential.as_ref().map(|(k, _)| k.clone()))
        .bind(credential.as_ref().map(|(_, e)| e.clone()))
        .execute(pool)
        .await?;
    Ok(())
}

/// Store (or replace) an org's fallback credential for unattended runs.
///
/// `org_credentials` has been read at dispatch since M6 but had no writer,
/// so every scheduled run silently fell through to the operator's global
/// key. This is that writer.
pub async fn put_org_credential(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    kind: &str,
    value_enc: Vec<u8>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let (stored,): (Uuid,) = sqlx::query_as(
        "INSERT INTO org_credentials (id, org_id, user_id, kind, value_enc, expires_at)          VALUES ($1,$2,$3,$4,$5,$6)          ON CONFLICT (org_id, user_id, kind) DO UPDATE            SET value_enc = EXCLUDED.value_enc, expires_at = EXCLUDED.expires_at,                created_at = now()          RETURNING id",
    )
    .bind(id)
    .bind(org_id)
    .bind(user_id)
    .bind(kind)
    .bind(&value_enc)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .context("storing the org credential")?;
    Ok(stored)
}

/// Metadata only — the encrypted value is never returned to a caller.
pub async fn list_org_credentials(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<(Uuid, String, Option<DateTime<Utc>>, DateTime<Utc>)>> {
    Ok(sqlx::query_as(
        "SELECT id, kind, expires_at, created_at FROM org_credentials          WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

pub async fn delete_org_credential(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<bool> {
    let n = sqlx::query("DELETE FROM org_credentials WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

/// The org's stored fallback credential, for unattended (scheduled) runs
/// where there is no live caller to borrow a bearer from.
/// Every live credential an org holds, newest first.
///
/// `org_credential` answers "the one to use" for callers that just need a
/// value; the dispatcher needs the whole set, because a refresh token and a
/// cached bearer minted from it coexist and the choice between them depends
/// on whether that bearer is still fresh.
/// One row of `org_credentials`: kind, encrypted value, expiry, owner.
///
/// `expires_at` is load-bearing rather than informational -- see
/// `api::usable_bearer`, which uses its absence to tell a hand-stored bearer
/// from one minted out of a refresh token.
pub type OrgCredentialRow = (String, Vec<u8>, Option<DateTime<Utc>>, Option<Uuid>);

pub async fn org_credentials_all(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<OrgCredentialRow>> {
    Ok(sqlx::query_as(
        "SELECT kind, value_enc, expires_at, user_id FROM org_credentials \
         WHERE org_id = $1 AND (expires_at IS NULL OR expires_at > now()) \
         ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

#[allow(dead_code)] // kept for callers that want one value without minting
pub async fn org_credential(pool: &PgPool, org_id: Uuid) -> Result<Option<(String, Vec<u8>)>> {
    // Prefer a non-expiring api_key over a bearer that may already be dead.
    Ok(sqlx::query_as(
        "SELECT kind, value_enc FROM org_credentials \
         WHERE org_id = $1 AND (expires_at IS NULL OR expires_at > now()) \
         ORDER BY (kind = 'api_key') DESC LIMIT 1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_session(pool: &PgPool, id: Uuid) -> Result<Option<SessionRow>> {
    Ok(sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {SESSION_COLS} FROM sessions WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// `user_id: Some(u)` lists that user's sessions (plus legacy rows with no
/// owner, which would otherwise become invisible to everyone); `None` lists
/// the whole org.
pub async fn list_sessions(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<SessionRow>> {
    Ok(sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {SESSION_COLS} FROM sessions WHERE org_id = $1 \
         AND ($2::uuid IS NULL OR user_id = $2 OR user_id IS NULL) \
         AND ($3::text IS NULL OR state = $3) \
         ORDER BY created_at DESC LIMIT $4"
    ))
    .bind(org_id)
    .bind(user_id)
    .bind(state)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Sessions assigned to a worker: everything from dispatch to park.
///
/// `scheduled` is included so a session that has been placed but has not
/// booted yet is visible rather than appearing to vanish between the
/// dispatcher and the first state frame. Callers doing drift detection must
/// skip it — no VM exists for it yet.
///
/// `org_id: None` is the admin view across all tenants.
pub async fn live_sessions(
    pool: &PgPool,
    org_id: Option<Uuid>,
    user_id: Option<Uuid>,
) -> Result<Vec<SessionRow>> {
    Ok(sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {SESSION_COLS} FROM sessions \
         WHERE state IN ('scheduled','booting','bootstrapping','running','waiting_input','stopping') \
           AND ($1::uuid IS NULL OR org_id = $1) \
           AND ($2::uuid IS NULL OR user_id = $2 OR user_id IS NULL) \
         ORDER BY started_at NULLS LAST"
    ))
    .bind(org_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Sessions that opened a given pull request.
///
/// Accepts a full URL or a bare number: `--from-pr 128` is what a person
/// types, and matching `%/pull/128` keeps that working without asking them
/// which repo they meant. Newest first, because a PR can be revisited by
/// more than one session.
pub async fn sessions_for_pr(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    pr: &str,
) -> Result<Vec<SessionRow>> {
    let pattern = if pr.starts_with("http") {
        pr.to_string()
    } else if pr.chars().all(|c| c.is_ascii_digit()) {
        format!("%/pull/{pr}")
    } else {
        format!("%{pr}%")
    };
    Ok(sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {SESSION_COLS} FROM sessions WHERE org_id = $1 \
         AND ($2::uuid IS NULL OR user_id = $2 OR user_id IS NULL) \
         AND pr_url IS NOT NULL AND pr_url LIKE $3 \
         ORDER BY created_at DESC LIMIT 20"
    ))
    .bind(org_id)
    .bind(user_id)
    .bind(pattern)
    .fetch_all(pool)
    .await?)
}

/// Sessions waiting for a worker: fresh ones, plus scheduled ones whose
/// assignment was never delivered (worker vanished before pickup).
pub async fn dispatchable_sessions(pool: &PgPool) -> Result<Vec<SessionRow>> {
    Ok(sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {SESSION_COLS} FROM sessions \
         WHERE worker_id IS NULL AND state IN ('created', 'scheduled') \
         ORDER BY created_at ASC LIMIT 20"
    ))
    .fetch_all(pool)
    .await?)
}

/// Allocate the next `count` seq numbers for a session inside `tx`, which
/// must hold the sessions row lock (`FOR UPDATE`).
async fn lock_session(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<(i64, i64, String)> {
    let (last_seq, last_guest_line, state): (i64, i64, String) = sqlx::query_as(
        "SELECT last_seq, last_guest_line, state FROM sessions WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .context("session not found")?;
    Ok((last_seq, last_guest_line, state))
}

async fn insert_event_row(tx: &mut Transaction<'_, Postgres>, ev: &Event) -> Result<()> {
    sqlx::query(
        "INSERT INTO session_events (session_id, seq, ts, kind, payload, guest_line, blob_ref) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(ev.session_id)
    .bind(ev.seq)
    .bind(ev.ts)
    .bind(ev.kind.as_str())
    .bind(&ev.payload)
    .bind(ev.guest_line)
    .bind(&ev.blob_ref)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Persist a batch of guest-originated events. Lines at or below the
/// session's guest cursor are silently dropped (idempotent redelivery).
/// Returns the events actually persisted, with global seq assigned.
pub async fn insert_guest_events(
    pool: &PgPool,
    session_id: Uuid,
    events: &[GuestEvent],
) -> Result<Vec<Event>> {
    if events.is_empty() {
        return Ok(vec![]);
    }
    let mut tx = pool.begin().await?;
    let (mut seq, mut cursor, _) = lock_session(&mut tx, session_id).await?;
    let mut out = Vec::new();
    for ge in events {
        if ge.line <= cursor {
            continue;
        }
        seq += 1;
        cursor = ge.line;
        let ev = Event {
            session_id,
            seq,
            ts: ge.ts,
            kind: ge.kind,
            payload: ge.payload.clone(),
            guest_line: Some(ge.line),
            blob_ref: ge.blob_ref.clone(),
        };
        insert_event_row(&mut tx, &ev).await?;
        out.push(ev);
    }
    sqlx::query("UPDATE sessions SET last_seq = $2, last_guest_line = $3 WHERE id = $1")
        .bind(session_id)
        .bind(seq)
        .bind(cursor)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(out)
}

/// Append one platform-originated event (kind session/user/exec).
pub async fn insert_platform_event(
    pool: &PgPool,
    session_id: Uuid,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<Event> {
    let mut tx = pool.begin().await?;
    let (last_seq, _, _) = lock_session(&mut tx, session_id).await?;
    let ev = Event {
        session_id,
        seq: last_seq + 1,
        ts: Utc::now(),
        kind,
        payload,
        guest_line: None,
        blob_ref: None,
    };
    insert_event_row(&mut tx, &ev).await?;
    sqlx::query("UPDATE sessions SET last_seq = $2 WHERE id = $1")
        .bind(session_id)
        .bind(ev.seq)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ev)
}

pub async fn fetch_events(
    pool: &PgPool,
    session_id: Uuid,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<Event>> {
    /// (seq, ts, kind, payload, guest_line, blob_ref)
    type EventRow = (i64, DateTime<Utc>, String, serde_json::Value, Option<i64>, Option<String>);
    let rows: Vec<EventRow> =
        sqlx::query_as(
            "SELECT seq, ts, kind, payload, guest_line, blob_ref FROM session_events \
             WHERE session_id = $1 AND seq > $2 ORDER BY seq ASC LIMIT $3",
        )
        .bind(session_id)
        .bind(after_seq)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(seq, ts, kind, payload, guest_line, blob_ref)| Event {
            session_id,
            seq,
            ts,
            kind: EventKind::parse(&kind).unwrap_or(EventKind::Session),
            payload,
            guest_line,
            blob_ref,
        })
        .collect())
}

/// Transition the session state machine. Validates the edge, stamps
/// started_at/ended_at, records a `session.state` event, and returns the
/// updated row plus the event for fanout. A transition to the current state
/// is a no-op returning no event.
pub async fn transition(
    pool: &PgPool,
    session_id: Uuid,
    next: SessionState,
    error: Option<&str>,
) -> Result<(SessionRow, Option<Event>)> {
    transition_with_prompt(pool, session_id, next, error, None).await
}

/// Transition, optionally replacing the prompt in the same transaction.
///
/// One transaction because the two must not come apart. Updating the prompt
/// first and transitioning after leaves a refused resume with someone else's
/// prompt on the row -- the original instruction is simply gone. Doing it
/// the other way round races the dispatcher, which reads the row as soon as
/// the state is `scheduled` and would send the old prompt.
pub async fn transition_with_prompt(
    pool: &PgPool,
    session_id: Uuid,
    next: SessionState,
    error: Option<&str>,
    prompt: Option<&str>,
) -> Result<(SessionRow, Option<Event>)> {
    let mut tx = pool.begin().await?;
    let (last_seq, _, cur_str) = lock_session(&mut tx, session_id).await?;
    let cur = SessionState::parse(&cur_str).context("bad state in db")?;
    if cur == next {
        tx.commit().await?;
        let row = get_session(pool, session_id).await?.context("session vanished")?;
        return Ok((row, None));
    }
    if !cur.can_transition_to(next) {
        bail!("invalid transition {} -> {}", cur.as_str(), next.as_str());
    }
    let ev = Event {
        session_id,
        seq: last_seq + 1,
        ts: Utc::now(),
        kind: EventKind::Session,
        payload: serde_json::json!({
            "type": "session.state",
            "state": next.as_str(),
            "error": error,
        }),
        guest_line: None,
        blob_ref: None,
    };
    insert_event_row(&mut tx, &ev).await?;
    // Clearing pending_question on a terminal state: a session that has
    // ended is not waiting on anyone. Leaving it set makes finished
    // sessions look answerable in a list view and puts a stale question in
    // the terminal notification.
    //
    // NB: no `--` comments inside this string. The `\` continuations join
    // every line into one, so a line comment would swallow the rest of the
    // statement — which is exactly the bug this comment replaces.
    let row = sqlx::query_as::<_, SessionRow>(&format!(
        "UPDATE sessions SET state = $2, last_seq = $3, error = COALESCE($4, error), \
         started_at = CASE WHEN $2 = 'running' AND started_at IS NULL THEN now() ELSE started_at END, \
         ended_at = CASE WHEN $2 IN ('completed','failed','canceled','stopped') AND ended_at IS NULL \
                    THEN now() ELSE ended_at END, \
         pending_question = CASE WHEN $2 IN ('completed','failed','canceled','reaped') \
                    THEN NULL ELSE pending_question END, \
         prompt = COALESCE($5::text, prompt) \
         WHERE id = $1 RETURNING {SESSION_COLS}"
    ))
    .bind(session_id)
    .bind(next.as_str())
    .bind(ev.seq)
    .bind(error)
    .bind(prompt)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((row, Some(ev)))
}

/// Claim a session for a worker, and snapshot what it has spent so far.
///
/// Returns false when someone else already holds the claim.
///
/// The `worker_id IS NULL` predicate is the serialization point.
/// `dispatch_pending` has eight callers -- five of them on the request path --
/// so two of them routinely run at once, and both used to select the same
/// `created` session, claim it, and send an `AssignSession`. The second
/// `transition` is a no-op rather than an error (`cur == next` returns
/// `Ok(None)`), so nothing downstream noticed. The worker's duplicate-
/// assignment guard hid it in production, at the cost of a leaked capacity
/// slot per double dispatch; in the test suite it surfaced as an extra
/// buffered frame and a ~1-in-4 flake on the resume tests.
///
/// The snapshot keeps a resumed session's bill honest: every dispatch starts
/// a fresh sandbox whose puku-cli counts from zero, so `update_usage` adds
/// the baseline back on. A first dispatch snapshots zeroes, which is right.
pub async fn assign_worker(pool: &PgPool, session_id: Uuid, worker_id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE sessions SET worker_id = $2, \
         cost_baseline_usd = cost_usd, \
         tokens_in_baseline = tokens_in, \
         tokens_out_baseline = tokens_out, \
         cache_read_baseline = cache_read_tokens, \
         cache_write_baseline = cache_write_tokens \
         WHERE id = $1 AND worker_id IS NULL",
    )
        .bind(session_id)
        .bind(worker_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn set_puku_session_id(pool: &PgPool, session_id: Uuid, psid: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET puku_session_id = $2 WHERE id = $1 AND puku_session_id IS NULL")
        .bind(session_id)
        .bind(psid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_usage(
    pool: &PgPool,
    session_id: Uuid,
    cost_usd: f64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
) -> Result<()> {
    // Baseline + what this sandbox reports. The worker sends puku-cli's own
    // cumulative total, which restarts at zero every time a new sandbox
    // boots -- so a bare assignment loses everything spent before a resume,
    // and a bare `+=` double-counts every report within one sandbox.
    sqlx::query(
        "UPDATE sessions SET \
         cost_usd = cost_baseline_usd + $2::float8::numeric, \
         tokens_in = tokens_in_baseline + $3, \
         tokens_out = tokens_out_baseline + $4, \
         cache_read_tokens = cache_read_baseline + $5, \
         cache_write_tokens = cache_write_baseline + $6 \
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(cost_usd)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cache_read_tokens)
    .bind(cache_write_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_worker(
    pool: &PgPool,
    name: &str,
    capacity_slots: i32,
    msb_version: &str,
    token_id: Option<Uuid>,
    engines: &[String],
    features: &[String],
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    // COALESCE on token_id: a worker that reconnects with the legacy shared
    // secret (token_id NULL) must not erase the binding it already has.
    let (worker_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO workers (id, name, status, capacity_slots, msb_version, token_id, \
                              last_heartbeat_at, engines, features) \
         VALUES ($1, $2, 'online', $3, $4, $5, now(), $6, $7) \
         ON CONFLICT (name) DO UPDATE SET status = 'online', capacity_slots = $3, \
             msb_version = $4, token_id = COALESCE($5, workers.token_id), \
             last_heartbeat_at = now(), engines = $6, features = $7 \
         RETURNING id",
    )
    .bind(id)
    .bind(name)
    .bind(capacity_slots)
    .bind(msb_version)
    .bind(token_id)
    .bind(engines)
    .bind(features)
    .fetch_one(pool)
    .await?;
    Ok(worker_id)
}

pub async fn worker_heartbeat(pool: &PgPool, worker_id: Uuid, used_slots: i32) -> Result<()> {
    sqlx::query(
        "UPDATE workers SET used_slots = $2, last_heartbeat_at = now() WHERE id = $1",
    )
    .bind(worker_id)
    .bind(used_slots)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn worker_offline(pool: &PgPool, worker_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE workers SET status = 'offline' WHERE id = $1")
        .bind(worker_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn audit(
    pool: &PgPool,
    org_id: Option<Uuid>,
    actor_user_id: Option<Uuid>,
    action: &str,
    subject: &str,
    detail: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (org_id, actor_user_id, action, subject, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(org_id)
    .bind(actor_user_id)
    .bind(action)
    .bind(subject)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// Quota check for session creation. Returns Err(reason) when over quota.
pub async fn check_quota(pool: &PgPool, org_id: Uuid) -> Result<std::result::Result<(), String>> {
    let (max_concurrent, max_monthly): (i32, f64) = sqlx::query_as(
        "SELECT max_concurrent_sessions, max_monthly_usd::float8 FROM quotas WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or((100, 100.0));

    let (active,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM sessions WHERE org_id = $1 AND state IN \
         ('created','scheduled','booting','bootstrapping','running','waiting_input','stopping')",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    if active >= max_concurrent as i64 {
        return Ok(Err(format!(
            "concurrent session quota reached ({active}/{max_concurrent})"
        )));
    }

    let month_cost = month_spend(pool, org_id).await?;
    if month_cost >= max_monthly {
        return Ok(Err(format!(
            "monthly budget reached (${month_cost:.2}/${max_monthly:.2})"
        )));
    }
    Ok(Ok(()))
}

/// Month-to-date spend for an org, counting **in-flight** sessions as well
/// as closed ones.
///
/// `usage_records` is only written at terminal transition, so a query over
/// it alone reports a long-running session as free until the moment it
/// ends — which is exactly how a single run walks past a monthly cap. The
/// NOT EXISTS clause adds live sessions without double-counting the ones
/// already recorded.
pub async fn month_spend(pool: &PgPool, org_id: Uuid) -> Result<f64> {
    let (spend,): (f64,) = sqlx::query_as(
        "SELECT ( \
            COALESCE((SELECT sum(cost_usd) FROM usage_records \
                      WHERE org_id = $1 AND period >= date_trunc('month', now())::date), 0) \
          + COALESCE((SELECT sum(s.cost_usd) FROM sessions s \
                      WHERE s.org_id = $1 \
                        AND s.created_at >= date_trunc('month', now()) \
                        AND NOT EXISTS (SELECT 1 FROM usage_records u \
                                        WHERE u.session_id = s.id)), 0) \
         )::float8",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    Ok(spend)
}

/// The org's monthly cap, or the default when no quota row exists.
pub async fn monthly_cap(pool: &PgPool, org_id: Uuid) -> Result<f64> {
    let row: Option<(f64,)> =
        sqlx::query_as("SELECT max_monthly_usd::float8 FROM quotas WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(v,)| v).unwrap_or(100.0))
}

/// Write the per-session usage record at terminal transition (idempotent).
pub async fn record_usage(pool: &PgPool, session_id: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO usage_records (id, org_id, session_id, period, tokens_in, tokens_out, \
                                    cache_read_tokens, cache_write_tokens, cost_usd, vm_seconds) \
         SELECT $1, org_id, id, now()::date, tokens_in, tokens_out, \
                cache_read_tokens, cache_write_tokens, cost_usd, \
                COALESCE(EXTRACT(EPOCH FROM (ended_at - started_at))::bigint, 0) \
         FROM sessions WHERE id = $2 \
         ON CONFLICT (session_id) DO UPDATE SET \
            tokens_in = EXCLUDED.tokens_in, tokens_out = EXCLUDED.tokens_out, \
            cache_read_tokens = EXCLUDED.cache_read_tokens, \
            cache_write_tokens = EXCLUDED.cache_write_tokens, \
            cost_usd = EXCLUDED.cost_usd, vm_seconds = EXCLUDED.vm_seconds",
    )
    .bind(Uuid::new_v4())
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resume cursors (highest persisted guest_line) for the given sessions.
pub async fn resume_cursors(
    pool: &PgPool,
    session_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, i64>> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, last_guest_line FROM sessions WHERE id = ANY($1)",
    )
    .bind(session_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::title_from_prompt;

    #[test]
    fn short_prompts_are_their_own_title() {
        assert_eq!(title_from_prompt("fix the flaky test"), "fix the flaky test");
    }

    /// Prompts arrive with newlines and runs of spaces; a title is one line.
    #[test]
    fn whitespace_is_flattened() {
        assert_eq!(title_from_prompt("fix   the\n\nflaky test\n"), "fix the flaky test");
    }

    #[test]
    fn long_prompts_are_cut_on_a_word_boundary() {
        let prompt = "refactor the authentication middleware so that every \
                      request carries a verified principal";
        let title = title_from_prompt(prompt);
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= 61, "{title}");
        // The cut lands between words, never mid-word.
        assert!(!title.trim_end_matches('…').ends_with(' '));
        assert!(prompt.starts_with(title.trim_end_matches('…')));
    }

    /// One unbroken 60+ char token has no space to back off to. Cutting at
    /// the last space would leave an empty title, so keep the hard cut.
    #[test]
    fn unbroken_token_still_yields_a_title() {
        let title = title_from_prompt(&"x".repeat(200));
        assert_eq!(title.chars().count(), 61); // 60 + the ellipsis
    }
}

#[cfg(test)]
mod sandbox_name_tests {
    /// puku-workerd derives this name to reap a sandbox it has no session
    /// row for. If the two ever disagree, the reaper silently deletes
    /// nothing -- so the shape is pinned on both sides.
    #[test]
    fn the_name_is_ses_plus_twelve_hex() {
        let id = uuid::Uuid::parse_str("43ffa9ff-b166-43bb-8506-e5f3d54c2846").unwrap();
        let name = format!("ses-{}", &id.simple().to_string()[..12]);
        assert_eq!(name, "ses-43ffa9ffb166");
    }
}


/// Pin the resolved profile and its preamble to the session.
///
/// Written once, at first dispatch. Every later dispatch of the same session
/// (a resume, a continued conversation) replays what is stored, because a
/// system prompt that changes underneath a resumed transcript is a whole class
/// of unreproducible behaviour.
pub async fn set_memory_preamble(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    profile_id: &str,
    preamble: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE sessions SET memory_profile_id = $2, memory_preamble = $3 WHERE id = $1",
    )
    .bind(session_id)
    .bind(profile_id)
    .bind(preamble)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record that a session's transcript reached the memory service.
pub async fn mark_memory_ingested(pool: &sqlx::PgPool, session_id: Uuid) -> anyhow::Result<()> {
    sqlx::query("UPDATE sessions SET memory_ingested_at = now() WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Is memory switched on for this session's org?
pub async fn memory_enabled(pool: &sqlx::PgPool, org_id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT memory_enabled FROM orgs WHERE id = $1")
        .bind(org_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Store the last preamble we successfully fetched for a profile.
pub async fn cache_preamble(
    pool: &sqlx::PgPool,
    profile_id: &str,
    preamble: &str,
    etag: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO memory_preamble_cache (profile_id, preamble, etag, fetched_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (profile_id) DO UPDATE SET preamble = EXCLUDED.preamble, \
             etag = EXCLUDED.etag, fetched_at = now()",
    )
    .bind(profile_id)
    .bind(preamble)
    .bind(etag)
    .execute(pool)
    .await?;
    Ok(())
}

/// The fallback that keeps the degradation ladder intact across the network
/// hop: when the memory service is slow or down, dispatch serves this instead
/// of nothing.
pub async fn cached_preamble(pool: &sqlx::PgPool, profile_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT preamble FROM memory_preamble_cache WHERE profile_id = $1",
    )
    .bind(profile_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Events worth distilling: the transcript minus anything spilled to a blob.
pub async fn events_for_memory(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    limit: i64,
) -> anyhow::Result<Vec<crate::memory::distill::Event>> {
    let rows: Vec<(serde_json::Value, Option<String>)> = sqlx::query_as(
        // The LAST `limit` events, returned in order.
        //
        // `ORDER BY seq LIMIT` took the first, and a busy session emits well
        // over the limit in its opening hour -- every tool call is several
        // events -- so a five-hour session was distilled from roughly its
        // first hour. Combined with distill()'s message cap, the end of a long
        // session never reached memory at all.
        "SELECT payload, blob_ref FROM ( \
           SELECT payload, blob_ref, seq FROM session_events \
            WHERE session_id = $1 AND kind = 'agent' \
            ORDER BY seq DESC LIMIT $2 \
         ) t ORDER BY seq ASC",
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(payload, blob_ref)| crate::memory::distill::Event { payload, blob_ref })
        .collect())
}

/// One memory operation, rolled up per org per day.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryOp {
    /// Preamble fetches. NOT recalls -- serving retrieves nothing; this is two
    /// indexed reads against the memory service's Postgres.
    pub preamble_fetches: i64,
    pub preamble_failures: i64,
    pub preamble_ms: i64,
    pub ingests: i64,
    pub ingest_failures: i64,
    pub messages_sent: i64,
    pub preamble_bytes: i64,
}

/// Record memory traffic (F28).
///
/// What this counts is traffic between controld and the memory service:
/// preamble fetches on the way in, ingests on the way out. Neither reaches
/// Cloudflare -- extraction is local and serving retrieves nothing -- so this
/// is a load signal, not a bill. Model spend is metered inside the memory
/// service, which is the only place that knows the token counts.
///
/// Best-effort: a metering failure must never affect the operation it meters.
pub async fn record_memory_usage(pool: &PgPool, org_id: Uuid, op: MemoryOp) {
    let res = sqlx::query(
        "INSERT INTO memory_usage (org_id, period, preamble_fetches, preamble_failures, preamble_ms, \
                                   ingests, ingest_failures, messages_sent, preamble_bytes) \
         VALUES ($1, now()::date, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (org_id, period) DO UPDATE SET \
            preamble_fetches  = memory_usage.preamble_fetches  + EXCLUDED.preamble_fetches, \
            preamble_failures = memory_usage.preamble_failures + EXCLUDED.preamble_failures, \
            preamble_ms       = memory_usage.preamble_ms       + EXCLUDED.preamble_ms, \
            ingests         = memory_usage.ingests         + EXCLUDED.ingests, \
            ingest_failures = memory_usage.ingest_failures + EXCLUDED.ingest_failures, \
            messages_sent   = memory_usage.messages_sent   + EXCLUDED.messages_sent, \
            preamble_bytes  = memory_usage.preamble_bytes  + EXCLUDED.preamble_bytes",
    )
    .bind(org_id)
    .bind(op.preamble_fetches)
    .bind(op.preamble_failures)
    .bind(op.preamble_ms)
    .bind(op.ingests)
    .bind(op.ingest_failures)
    .bind(op.messages_sent)
    .bind(op.preamble_bytes)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(%org_id, error = %e, "recording memory usage failed");
    }
}

/// This org's memory traffic today, for the dashboard and the API.
pub async fn memory_usage_today(pool: &PgPool, org_id: Uuid) -> Option<serde_json::Value> {
    sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64)>(
        "SELECT preamble_fetches, preamble_failures, preamble_ms, ingests, ingest_failures, \
                messages_sent, preamble_bytes \
           FROM memory_usage WHERE org_id = $1 AND period = now()::date",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(r, rf, rms, i, if_, m, pb)| {
        serde_json::json!({
            "preamble_fetches": r, "preamble_failures": rf,
            "preamble_ms_avg": if r > 0 { rms / r } else { 0 },
            "ingests": i, "ingest_failures": if_,
            "messages_sent": m, "preamble_bytes": pb,
        })
    })
}
