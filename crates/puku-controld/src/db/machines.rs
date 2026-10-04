//! Machine rows (migrations/0022_machines.sql).
//!
//! Every write that a worker frame drives is a compare-and-set on the boot's
//! `generation` and the reporting worker, so a late frame from a previous
//! boot -- or from a worker the machine has since moved off -- changes
//! nothing.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use puku_cloud_proto::machine::{machine_name_for, MachineState};
use puku_cloud_proto::Engine;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

const MACHINE_COLS: &str = "id, org_id, user_id, external_id, name, engine, image, cpus, \
    memory_mib, expose, env, entrypoint, volume, labels, idle_timeout_s, max_duration_s, \
    state, generation, worker_id, volume_worker_id, volume_existed, error, created_at, \
    started_at, stopped_at, last_active_at, last_reason, persist_root, snapshot_policy, \
    latest_snapshot_id, restore_snapshot_id, stale_worker_id";

#[derive(Debug, Clone, FromRow)]
pub struct MachineRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub external_id: Option<String>,
    pub name: String,
    pub engine: String,
    pub image: String,
    pub cpus: i32,
    pub memory_mib: i32,
    pub expose: Vec<i32>,
    /// Plain env. The secret half lives in `secret_env_enc`, which is not
    /// part of this row on purpose: nothing that serializes a machine can
    /// reach it.
    pub env: serde_json::Value,
    pub entrypoint: Option<serde_json::Value>,
    pub volume: Option<serde_json::Value>,
    pub labels: serde_json::Value,
    pub idle_timeout_s: i32,
    pub max_duration_s: i32,
    pub state: String,
    pub generation: i64,
    pub worker_id: Option<Uuid>,
    pub volume_worker_id: Option<Uuid>,
    pub volume_existed: bool,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub last_active_at: DateTime<Utc>,
    /// Why it is not running, as a word a client can act on
    /// (`capacity_full`, `image_not_staged`, ...). `error` has the sentence.
    pub last_reason: Option<String>,
    pub persist_root: bool,
    /// `snapshots::SnapshotPolicy`, as the caller set it.
    pub snapshot_policy: serde_json::Value,
    pub latest_snapshot_id: Option<Uuid>,
    /// The snapshot this boot restores (or restored) from.
    pub restore_snapshot_id: Option<Uuid>,
    /// A worker still holding the copy a restore replaced.
    pub stale_worker_id: Option<Uuid>,
}

impl MachineRow {
    pub fn machine_state(&self) -> MachineState {
        MachineState::parse(&self.state).unwrap_or(MachineState::Failed)
    }

    pub fn engine(&self) -> Engine {
        Engine::parse(&self.engine).unwrap_or(Engine::Unsupported)
    }

    pub fn exposes(&self, port: u16) -> bool {
        self.expose.contains(&(port as i32))
    }

    /// The machine as the API returns it.
    pub fn to_api(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "external_id": self.external_id,
            "name": self.name,
            "state": self.state,
            "engine": self.engine,
            "image": self.image,
            "cpus": self.cpus,
            "memory_mib": self.memory_mib,
            "expose": self.expose,
            "volume": self.volume,
            "labels": self.labels,
            "worker_id": self.worker_id,
            "error": self.error,
            "reason": self.last_reason,
            "persist_root": self.persist_root,
            "snapshots": self.snapshot_policy,
            "latest_snapshot_id": self.latest_snapshot_id,
            "restored_from": self.restore_snapshot_id,
            "idle_timeout_s": self.idle_timeout_s,
            "max_duration_s": self.max_duration_s,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "stopped_at": self.stopped_at,
            "last_active_at": self.last_active_at,
        })
    }
}

/// The caller-controlled half of a machine: what create writes and what an
/// ensure-running POST replaces.
#[derive(Debug, Clone)]
pub struct MachineFields {
    pub engine: Engine,
    pub image: String,
    pub cpus: i32,
    pub memory_mib: i32,
    pub expose: Vec<i32>,
    pub env: serde_json::Value,
    pub secret_env_enc: Option<Vec<u8>>,
    pub entrypoint: Option<serde_json::Value>,
    pub volume: Option<serde_json::Value>,
    pub labels: serde_json::Value,
    pub idle_timeout_s: i32,
    pub max_duration_s: i32,
    pub persist_root: bool,
    pub snapshot_policy: serde_json::Value,
}

pub async fn insert(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    external_id: Option<&str>,
    f: &MachineFields,
) -> Result<MachineRow> {
    let id = Uuid::new_v4();
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "INSERT INTO machines (id, org_id, user_id, external_id, name, engine, image, cpus, \
             memory_mib, expose, env, secret_env_enc, entrypoint, volume, labels, \
             idle_timeout_s, max_duration_s, persist_root, snapshot_policy) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19) \
         RETURNING {MACHINE_COLS}"
    ))
    .bind(id)
    .bind(org_id)
    .bind(user_id)
    .bind(external_id)
    .bind(machine_name_for(id))
    .bind(f.engine.as_str())
    .bind(&f.image)
    .bind(f.cpus)
    .bind(f.memory_mib)
    .bind(&f.expose)
    .bind(&f.env)
    .bind(&f.secret_env_enc)
    .bind(&f.entrypoint)
    .bind(&f.volume)
    .bind(&f.labels)
    .bind(f.idle_timeout_s)
    .bind(f.max_duration_s)
    .bind(f.persist_root)
    .bind(&f.snapshot_policy)
    .fetch_one(pool)
    .await?)
}

/// Replace the spec of an existing machine. It takes effect at the next
/// boot; the engine is kept, because a volume laid out for one guest is not
/// something to boot another under.
pub async fn update_fields(pool: &PgPool, id: Uuid, f: &MachineFields) -> Result<MachineRow> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "UPDATE machines SET image = $2, cpus = $3, memory_mib = $4, expose = $5, env = $6, \
             secret_env_enc = $7, entrypoint = $8, volume = $9, labels = $10, \
             idle_timeout_s = $11, max_duration_s = $12, persist_root = $13, snapshot_policy = $14 \
         WHERE id = $1 RETURNING {MACHINE_COLS}"
    ))
    .bind(id)
    .bind(&f.image)
    .bind(f.cpus)
    .bind(f.memory_mib)
    .bind(&f.expose)
    .bind(&f.env)
    .bind(&f.secret_env_enc)
    .bind(&f.entrypoint)
    .bind(&f.volume)
    .bind(&f.labels)
    .bind(f.idle_timeout_s)
    .bind(f.max_duration_s)
    .bind(f.persist_root)
    .bind(&f.snapshot_policy)
    .fetch_one(pool)
    .await?)
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!("SELECT {MACHINE_COLS} FROM machines WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// The live machine an org's `external_id` names, if any.
pub async fn get_by_external(
    pool: &PgPool,
    org_id: Uuid,
    external_id: &str,
) -> Result<Option<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines \
         WHERE org_id = $1 AND external_id = $2 AND state <> 'destroyed'"
    ))
    .bind(org_id)
    .bind(external_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    external_id: Option<&str>,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines WHERE org_id = $1 \
         AND ($2::uuid IS NULL OR user_id = $2 OR user_id IS NULL) \
         AND ($3::text IS NULL OR external_id = $3) \
         AND ($4::text IS NULL OR state = $4) \
         ORDER BY created_at DESC LIMIT $5"
    ))
    .bind(org_id)
    .bind(user_id)
    .bind(external_id)
    .bind(state)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// The sealed secret env, read only at dispatch.
pub async fn secret_env(pool: &PgPool, id: Uuid) -> Result<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT secret_env_enc FROM machines WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .flatten())
}

/// Queue a new boot of a stopped or failed machine. `None` when it is in
/// any other state (already live, or destroyed).
pub async fn schedule_start(pool: &PgPool, id: Uuid) -> Result<Option<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "UPDATE machines SET state = 'scheduled', generation = generation + 1, \
             worker_id = NULL, error = NULL, last_reason = NULL, restore_snapshot_id = NULL, \
             last_active_at = now() \
         WHERE id = $1 AND state IN ('stopped', 'failed') RETURNING {MACHINE_COLS}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Queue a boot that first restores a snapshot. `pinned` is where the
/// restore must land, or `None` for anywhere; the worker holding the copy it
/// replaces is remembered, so it can be told to drop that copy once the
/// restored boot runs.
pub async fn schedule_restore(
    pool: &PgPool,
    id: Uuid,
    snapshot_id: Uuid,
    pinned: Option<Uuid>,
    cpus: i32,
    memory_mib: i32,
) -> Result<Option<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "UPDATE machines SET state = 'scheduled', generation = generation + 1, worker_id = NULL, \
             error = NULL, last_reason = NULL, last_active_at = now(), restore_snapshot_id = $2, \
             stale_worker_id = CASE WHEN volume_worker_id IS DISTINCT FROM $3 \
                                    THEN volume_worker_id ELSE stale_worker_id END, \
             volume_worker_id = $3, cpus = $4, memory_mib = $5 \
         WHERE id = $1 AND state IN ('stopped', 'failed') RETURNING {MACHINE_COLS}"
    ))
    .bind(id)
    .bind(snapshot_id)
    .bind(pinned)
    .bind(cpus)
    .bind(memory_mib)
    .fetch_optional(pool)
    .await?)
}

/// Re-point a scheduled boot whose volume host is gone for good at a
/// snapshot, so it comes back with its files rather than an empty volume.
/// False when the boot was placed or moved on meanwhile.
pub async fn relocate_scheduled(pool: &PgPool, id: Uuid, generation: i64, snapshot_id: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE machines SET restore_snapshot_id = $3, stale_worker_id = volume_worker_id, \
             volume_worker_id = NULL \
         WHERE id = $1 AND generation = $2 AND state = 'scheduled' AND worker_id IS NULL",
    )
    .bind(id)
    .bind(generation)
    .bind(snapshot_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn clear_stale(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE machines SET stale_worker_id = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Running machines whose periodic snapshot is due: none attempted within
/// their interval. Counting attempts, not successes, is what stops a machine
/// whose snapshots keep failing from being retried on every sweep.
pub async fn due_for_snapshot(pool: &PgPool) -> Result<Vec<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines m \
         WHERE m.state = 'running' \
           AND COALESCE((m.snapshot_policy->>'interval_s')::int, 0) > 0 \
           AND NOT EXISTS ( \
               SELECT 1 FROM machine_snapshots s WHERE s.machine_id = m.id \
                 AND s.created_at > now() - make_interval(secs => (m.snapshot_policy->>'interval_s')::int)) \
         LIMIT 50"
    ))
    .fetch_all(pool)
    .await?)
}

/// Ask a live machine to stop. One that was never placed stops on the spot;
/// one on a worker goes to `stopping` until the worker confirms. `None` when
/// it is not live.
pub async fn request_stop(pool: &PgPool, id: Uuid) -> Result<Option<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "UPDATE machines SET \
             state = CASE WHEN worker_id IS NULL THEN 'stopped' ELSE 'stopping' END, \
             stopped_at = CASE WHEN worker_id IS NULL THEN now() ELSE stopped_at END \
         WHERE id = $1 AND state IN ('scheduled', 'restoring', 'booting', 'running') \
         RETURNING {MACHINE_COLS}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Mark a machine destroyed and return it as it was, so the caller knows
/// which worker to tell.
pub async fn mark_destroyed(pool: &PgPool, id: Uuid) -> Result<Option<MachineRow>> {
    let mut tx = pool.begin().await?;
    let before = sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if before.is_some() {
        sqlx::query(
            "UPDATE machines SET state = 'destroyed', destroyed_at = now(), \
                 stopped_at = COALESCE(stopped_at, now()), worker_id = NULL \
             WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(before)
}

/// Machines waiting for a worker, oldest first.
pub async fn dispatchable(pool: &PgPool) -> Result<Vec<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines \
         WHERE state = 'scheduled' AND worker_id IS NULL \
         ORDER BY created_at ASC LIMIT 20"
    ))
    .fetch_all(pool)
    .await?)
}

/// Claim a scheduled machine's current boot for a worker. False when another
/// dispatcher got there first or the boot moved on.
pub async fn assign_worker(pool: &PgPool, id: Uuid, worker_id: Uuid, generation: i64) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE machines SET worker_id = $2 \
         WHERE id = $1 AND worker_id IS NULL AND state = 'scheduled' AND generation = $3",
    )
    .bind(id)
    .bind(worker_id)
    .bind(generation)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Undo a claim whose assignment never reached the worker.
pub async fn unassign(pool: &PgPool, id: Uuid, generation: i64) -> Result<()> {
    sqlx::query(
        "UPDATE machines SET worker_id = NULL \
         WHERE id = $1 AND generation = $2 AND state = 'scheduled'",
    )
    .bind(id)
    .bind(generation)
    .execute(pool)
    .await?;
    Ok(())
}

/// Put a boot the fleet could not place back to stopped, with why.
/// Compare-and-set on the boot and on nobody having placed it meanwhile:
/// false means a dispatcher got there first and the boot is going ahead.
pub async fn unschedule(pool: &PgPool, id: Uuid, generation: i64, reason: &str, message: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE machines SET state = 'stopped', stopped_at = now(), last_reason = $3, error = $4 \
         WHERE id = $1 AND generation = $2 AND state = 'scheduled' AND worker_id IS NULL",
    )
    .bind(id)
    .bind(generation)
    .bind(reason)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Record why a start was refused without changing the machine's state: it
/// stays stopped (or failed), now saying why.
pub async fn note_refusal(pool: &PgPool, id: Uuid, reason: &str, message: &str) -> Result<()> {
    sqlx::query(
        "UPDATE machines SET last_reason = $2, error = $3 \
         WHERE id = $1 AND state IN ('stopped', 'failed')",
    )
    .bind(id)
    .bind(reason)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record why a queued machine is still waiting. Writes only when the reason
/// changed: the dispatcher calls this on every tick.
pub async fn note_waiting(pool: &PgPool, id: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE machines SET last_reason = $2 \
         WHERE id = $1 AND state = 'scheduled' AND last_reason IS DISTINCT FROM $2",
    )
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forget where a machine's volume was: its worker has been gone past the
/// grace period, so the next boot starts from an empty one elsewhere.
pub async fn forget_volume(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE machines SET volume_worker_id = NULL, volume_existed = false WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Apply a worker's report about one boot. Returns the updated row, or
/// `None` when the report was stale (another generation, another worker, or
/// a machine already destroyed).
// One argument per column the report can touch, plus the three that fence
// it; a struct would only move the same list to three call sites.
#[allow(clippy::too_many_arguments)]
pub async fn apply_report(
    pool: &PgPool,
    id: Uuid,
    worker_id: Uuid,
    generation: i64,
    state: MachineState,
    error: Option<&str>,
    reason: Option<&str>,
    volume_existed: bool,
) -> Result<Option<MachineRow>> {
    let sql = match state {
        MachineState::Restoring => format!(
            "UPDATE machines SET state = 'restoring' \
             WHERE id = $1 AND worker_id = $2 AND generation = $3 AND state = 'scheduled' \
             RETURNING {MACHINE_COLS}"
        ),
        MachineState::Booting => format!(
            "UPDATE machines SET state = 'booting' \
             WHERE id = $1 AND worker_id = $2 AND generation = $3 \
               AND state IN ('scheduled', 'restoring') \
             RETURNING {MACHINE_COLS}"
        ),
        MachineState::Running => format!(
            "UPDATE machines SET state = 'running', started_at = now(), last_active_at = now(), \
                 error = NULL, last_reason = NULL, volume_existed = $5, \
                 volume_worker_id = COALESCE(volume_worker_id, $2), \
                 stale_worker_id = NULLIF(stale_worker_id, $2) \
             WHERE id = $1 AND worker_id = $2 AND generation = $3 \
               AND state IN ('scheduled', 'restoring', 'booting', 'running') \
             RETURNING {MACHINE_COLS}"
        ),
        MachineState::Stopped | MachineState::Failed => format!(
            "UPDATE machines SET state = $6, stopped_at = now(), worker_id = NULL, \
                 error = COALESCE($4, CASE WHEN $6 = 'failed' THEN error ELSE NULL END), \
                 last_reason = COALESCE($7, CASE WHEN $6 = 'failed' THEN last_reason ELSE NULL END) \
             WHERE id = $1 AND worker_id = $2 AND generation = $3 AND state <> 'destroyed' \
             RETURNING {MACHINE_COLS}"
        ),
        // Workers never originate these.
        MachineState::Scheduled | MachineState::Stopping | MachineState::Destroyed => {
            return Ok(None)
        }
    };
    sqlx::query_as::<_, MachineRow>(&sql)
        .bind(id)
        .bind(worker_id)
        .bind(generation)
        .bind(error)
        .bind(volume_existed)
        .bind(state.as_str())
        .bind(reason)
        .fetch_optional(pool)
        .await
        .context("applying a machine report")
}

/// Machines this worker was running when it went away, which it no longer
/// has: the VM died with it. Returned as they were, then stopped.
pub async fn stop_lost(pool: &PgPool, worker_id: Uuid, still_running: &[Uuid]) -> Result<Vec<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "UPDATE machines SET state = 'stopped', stopped_at = now(), worker_id = NULL, \
             error = 'the VM was lost while its worker was away', last_reason = 'vm_lost' \
         WHERE worker_id = $1 AND state IN ('restoring', 'booting', 'running', 'stopping') \
           AND NOT (id = ANY($2)) \
         RETURNING {MACHINE_COLS}"
    ))
    .bind(worker_id)
    .bind(still_running)
    .fetch_all(pool)
    .await?)
}

/// Hand back machines assigned to a worker that never booted them: the
/// assignment went down a link that is now gone, and nothing else would
/// ever re-send it. `except` is what the worker says it does run.
pub async fn release_unbooted(pool: &PgPool, worker_id: Uuid, except: &[Uuid]) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE machines SET worker_id = NULL \
         WHERE worker_id = $1 AND state = 'scheduled' AND NOT (id = ANY($2))",
    )
    .bind(worker_id)
    .bind(except)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Of the machine ids a worker holds volumes for, those it may delete:
/// destroyed, unknown here, or replaced by a restore that now runs elsewhere.
pub async fn reapable(pool: &PgPool, worker_id: Uuid, ids: &[Uuid]) -> Result<Vec<Uuid>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM machines WHERE id = ANY($1) AND state <> 'destroyed' \
           AND stale_worker_id IS DISTINCT FROM $2",
    )
    .bind(ids)
    .bind(worker_id)
    .fetch_all(pool)
    .await?;
    // Told once; nothing more to remember about those copies.
    sqlx::query("UPDATE machines SET stale_worker_id = NULL WHERE id = ANY($1) AND stale_worker_id = $2")
        .bind(ids)
        .bind(worker_id)
        .execute(pool)
        .await?;
    Ok(ids.iter().copied().filter(|id| !live.contains(id)).collect())
}

pub async fn open_run(pool: &PgPool, m: &MachineRow, worker_id: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO machine_runs (id, machine_id, org_id, worker_id, engine, generation, cpus, memory_mib) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (machine_id, generation) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(m.id)
    .bind(m.org_id)
    .bind(worker_id)
    .bind(&m.engine)
    .bind(m.generation)
    .bind(m.cpus)
    .bind(m.memory_mib)
    .execute(pool)
    .await?;
    Ok(())
}

/// Close every open run of a machine: whatever boot it was, it is over.
pub async fn close_runs(pool: &PgPool, machine_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE machine_runs SET ended_at = now() WHERE machine_id = $1 AND ended_at IS NULL")
        .bind(machine_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn touch(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE machines SET last_active_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Running machines idle past their own timeout. Machines with no timeout
/// (0, the default) are never here: their owner decides when they sleep.
pub async fn idle(pool: &PgPool) -> Result<Vec<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines \
         WHERE state = 'running' AND idle_timeout_s > 0 \
           AND last_active_at < now() - make_interval(secs => idle_timeout_s) \
         LIMIT 50"
    ))
    .fetch_all(pool)
    .await?)
}

/// Live machines on any worker, for the fleet view.
pub async fn live(pool: &PgPool, org_id: Option<Uuid>) -> Result<Vec<MachineRow>> {
    Ok(sqlx::query_as::<_, MachineRow>(&format!(
        "SELECT {MACHINE_COLS} FROM machines \
         WHERE state IN ('scheduled', 'restoring', 'booting', 'running', 'stopping') \
           AND ($1::uuid IS NULL OR org_id = $1) \
         ORDER BY created_at"
    ))
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

/// `(live machines, cap)` for an org's machine quota.
pub async fn quota(pool: &PgPool, org_id: Uuid) -> Result<(i64, i64)> {
    let cap: Option<i32> =
        sqlx::query_scalar("SELECT max_concurrent_machines FROM quotas WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    let (live,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM machines WHERE org_id = $1 AND state <> 'destroyed'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    Ok((live, cap.unwrap_or(20) as i64))
}
