//! Snapshot rows (migrations/0024_machine_snapshots.sql).
//!
//! A snapshot's rows are written before its worker uploads anything, so an
//! upload always has somewhere to report to, and the sweep can always find,
//! abort and clean up a capture that never finished.

use anyhow::Result;
use chrono::{DateTime, Utc};
use puku_cloud_proto::snapshot::{LayerOrder, SnapshotLayer, SnapshotTrigger};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use super::machines::MachineRow;

const SNAPSHOT_COLS: &str = "id, machine_id, trigger, state, consistency, worker_id, \
    engine, image, cpus, memory_mib, manifest, size_bytes, stored_bytes, dek_enc, \
    pinned, label, error, created_at, completed_at";

const OBJECT_COLS: &str = "layer, key, upload_id, part_bytes, parts, plain_bytes, stored_bytes, sha256, state";

#[derive(Debug, Clone, FromRow)]
pub struct SnapshotRow {
    pub id: Uuid,
    pub machine_id: Uuid,
    pub trigger: String,
    pub state: String,
    pub consistency: Option<String>,
    pub worker_id: Option<Uuid>,
    pub engine: String,
    pub image: String,
    pub cpus: i32,
    pub memory_mib: i32,
    pub manifest: Option<serde_json::Value>,
    pub size_bytes: Option<i64>,
    pub stored_bytes: Option<i64>,
    /// The data key, sealed. Never part of what the API returns.
    pub dek_enc: Vec<u8>,
    pub pinned: bool,
    pub label: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl SnapshotRow {
    /// The snapshot as the API returns it: no keys, sealed or otherwise.
    pub fn to_api(&self) -> serde_json::Value {
        let layers: Vec<serde_json::Value> = self
            .manifest
            .as_ref()
            .and_then(|m| m.get("layers"))
            .and_then(|l| l.as_array())
            .map(|ls| {
                ls.iter()
                    .map(|l| {
                        serde_json::json!({
                            "layer": l["layer"], "plain_bytes": l["plain_bytes"],
                            "stored_bytes": l["stored_bytes"], "reused": l["reused"],
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        serde_json::json!({
            "id": self.id,
            "machine_id": self.machine_id,
            "trigger": self.trigger,
            "state": self.state,
            "consistency": self.consistency,
            "engine": self.engine,
            "image": self.image,
            "cpus": self.cpus,
            "memory_mib": self.memory_mib,
            "size_bytes": self.size_bytes,
            "stored_bytes": self.stored_bytes,
            "layers": layers,
            "pinned": self.pinned,
            "label": self.label,
            "error": self.error,
            "created_at": self.created_at,
            "completed_at": self.completed_at,
        })
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state.as_str(), "pending" | "uploading")
    }
}

/// One layer's object.
#[derive(Debug, Clone, FromRow)]
pub struct ObjectRow {
    pub layer: String,
    pub key: String,
    pub upload_id: Option<String>,
    pub part_bytes: i64,
    pub parts: Option<i32>,
    pub plain_bytes: Option<i64>,
    pub stored_bytes: Option<i64>,
    pub sha256: Option<String>,
    pub state: String,
}

impl ObjectRow {
    pub fn layer(&self) -> Option<SnapshotLayer> {
        SnapshotLayer::parse(&self.layer)
    }

    /// Holds bytes a restore can read: uploaded here, or borrowed from an
    /// earlier snapshot.
    pub fn is_stored(&self) -> bool {
        matches!(self.state.as_str(), "complete" | "reused")
    }
}

pub struct NewSnapshot<'a> {
    pub id: Uuid,
    pub machine: &'a MachineRow,
    pub trigger: SnapshotTrigger,
    pub worker_id: Uuid,
    pub dek_enc: &'a [u8],
    pub pinned: bool,
    pub label: Option<&'a str>,
    pub part_bytes: i64,
    pub layers: &'a [LayerOrder],
}

pub async fn insert(pool: &PgPool, s: &NewSnapshot<'_>) -> Result<SnapshotRow> {
    let m = s.machine;
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, SnapshotRow>(&format!(
        "INSERT INTO machine_snapshots (id, machine_id, org_id, trigger, worker_id, generation, \
             engine, image, cpus, memory_mib, dek_enc, pinned, label) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING {SNAPSHOT_COLS}"
    ))
    .bind(s.id)
    .bind(m.id)
    .bind(m.org_id)
    .bind(s.trigger.as_str())
    .bind(s.worker_id)
    .bind(m.generation)
    .bind(&m.engine)
    .bind(&m.image)
    .bind(m.cpus)
    .bind(m.memory_mib)
    .bind(s.dek_enc)
    .bind(s.pinned)
    .bind(s.label)
    .fetch_one(&mut *tx)
    .await?;
    for l in s.layers {
        sqlx::query(
            "INSERT INTO machine_snapshot_objects (snapshot_id, layer, key, part_bytes) VALUES ($1,$2,$3,$4)",
        )
        .bind(s.id)
        .bind(l.layer.as_str())
        .bind(&l.key)
        .bind(s.part_bytes)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row)
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<SnapshotRow>> {
    Ok(sqlx::query_as::<_, SnapshotRow>(&format!("SELECT {SNAPSHOT_COLS} FROM machine_snapshots WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// A machine's snapshots, newest first, leaving out the deleted.
pub async fn list(pool: &PgPool, machine_id: Uuid, limit: i64) -> Result<Vec<SnapshotRow>> {
    Ok(sqlx::query_as::<_, SnapshotRow>(&format!(
        "SELECT {SNAPSHOT_COLS} FROM machine_snapshots \
         WHERE machine_id = $1 AND state <> 'deleted' ORDER BY created_at DESC LIMIT $2"
    ))
    .bind(machine_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn latest_ready(pool: &PgPool, machine_id: Uuid) -> Result<Option<SnapshotRow>> {
    Ok(sqlx::query_as::<_, SnapshotRow>(&format!(
        "SELECT {SNAPSHOT_COLS} FROM machine_snapshots \
         WHERE machine_id = $1 AND state = 'ready' ORDER BY created_at DESC LIMIT 1"
    ))
    .bind(machine_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn objects(pool: &PgPool, snapshot_id: Uuid) -> Result<Vec<ObjectRow>> {
    Ok(sqlx::query_as::<_, ObjectRow>(&format!(
        "SELECT {OBJECT_COLS} FROM machine_snapshot_objects WHERE snapshot_id = $1 ORDER BY layer"
    ))
    .bind(snapshot_id)
    .fetch_all(pool)
    .await?)
}

pub async fn object(pool: &PgPool, snapshot_id: Uuid, layer: SnapshotLayer) -> Result<Option<ObjectRow>> {
    Ok(sqlx::query_as::<_, ObjectRow>(&format!(
        "SELECT {OBJECT_COLS} FROM machine_snapshot_objects WHERE snapshot_id = $1 AND layer = $2"
    ))
    .bind(snapshot_id)
    .bind(layer.as_str())
    .fetch_optional(pool)
    .await?)
}

pub async fn set_upload_id(pool: &PgPool, snapshot_id: Uuid, layer: SnapshotLayer, upload_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE machine_snapshot_objects SET upload_id = $3, state = 'uploading' \
         WHERE snapshot_id = $1 AND layer = $2",
    )
    .bind(snapshot_id)
    .bind(layer.as_str())
    .bind(upload_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct Completed<'a> {
    pub parts: i32,
    pub plain_bytes: i64,
    pub stored_bytes: i64,
    pub sha256: &'a str,
}

pub async fn complete_object(pool: &PgPool, snapshot_id: Uuid, layer: SnapshotLayer, c: &Completed<'_>) -> Result<()> {
    sqlx::query(
        "UPDATE machine_snapshot_objects SET state = 'complete', parts = $3, plain_bytes = $4, \
             stored_bytes = $5, sha256 = $6 \
         WHERE snapshot_id = $1 AND layer = $2",
    )
    .bind(snapshot_id)
    .bind(layer.as_str())
    .bind(c.parts)
    .bind(c.plain_bytes)
    .bind(c.stored_bytes)
    .bind(c.sha256)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn fail_object(pool: &PgPool, snapshot_id: Uuid, layer: &str) -> Result<()> {
    sqlx::query("UPDATE machine_snapshot_objects SET state = 'failed' WHERE snapshot_id = $1 AND layer = $2")
        .bind(snapshot_id)
        .bind(layer)
        .execute(pool)
        .await?;
    Ok(())
}

/// Point a layer at an earlier snapshot's object instead of its own.
pub async fn reuse_object(pool: &PgPool, snapshot_id: Uuid, from: &ObjectRow) -> Result<()> {
    sqlx::query(
        "UPDATE machine_snapshot_objects SET state = 'reused', key = $3, upload_id = NULL, \
             parts = $4, plain_bytes = $5, stored_bytes = $6, sha256 = $7, part_bytes = $8 \
         WHERE snapshot_id = $1 AND layer = $2",
    )
    .bind(snapshot_id)
    .bind(&from.layer)
    .bind(&from.key)
    .bind(from.parts)
    .bind(from.plain_bytes)
    .bind(from.stored_bytes)
    .bind(&from.sha256)
    .bind(from.part_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

/// A layer this snapshot turned out not to have.
pub async fn drop_object(pool: &PgPool, snapshot_id: Uuid, layer: &str) -> Result<()> {
    sqlx::query("DELETE FROM machine_snapshot_objects WHERE snapshot_id = $1 AND layer = $2")
        .bind(snapshot_id)
        .bind(layer)
        .execute(pool)
        .await?;
    Ok(())
}

/// The newest stored object for a layer among a machine's ready snapshots,
/// other than `except`: what an unchanged layer is reused from.
pub async fn reusable_layer(
    pool: &PgPool,
    machine_id: Uuid,
    layer: SnapshotLayer,
    except: Uuid,
) -> Result<Option<ObjectRow>> {
    Ok(sqlx::query_as::<_, ObjectRow>(
        "SELECT o.layer, o.key, o.upload_id, o.part_bytes, o.parts, o.plain_bytes, \
                o.stored_bytes, o.sha256, o.state \
         FROM machine_snapshot_objects o JOIN machine_snapshots s ON s.id = o.snapshot_id \
         WHERE s.machine_id = $1 AND s.state = 'ready' AND o.layer = $2 \
           AND o.state IN ('complete', 'reused') AND s.id <> $3 \
         ORDER BY s.created_at DESC LIMIT 1",
    )
    .bind(machine_id)
    .bind(layer.as_str())
    .bind(except)
    .fetch_optional(pool)
    .await?)
}

/// Move an open snapshot on. `None` when it was no longer open: a late or
/// duplicate report, or one for a snapshot purged meanwhile.
pub async fn set_state(pool: &PgPool, id: Uuid, state: &str, error: Option<&str>) -> Result<Option<SnapshotRow>> {
    Ok(sqlx::query_as::<_, SnapshotRow>(&format!(
        "UPDATE machine_snapshots SET state = $2, error = $3, \
             completed_at = CASE WHEN $2 IN ('skipped', 'failed') THEN now() ELSE completed_at END \
         WHERE id = $1 AND state IN ('pending', 'uploading') RETURNING {SNAPSHOT_COLS}"
    ))
    .bind(id)
    .bind(state)
    .bind(error)
    .fetch_optional(pool)
    .await?)
}

pub struct Ready<'a> {
    pub consistency: &'a str,
    pub manifest: &'a serde_json::Value,
    pub size_bytes: i64,
    pub stored_bytes: i64,
}

/// Mark a capture ready and make it the machine's newest.
pub async fn mark_ready(pool: &PgPool, id: Uuid, r: &Ready<'_>) -> Result<Option<SnapshotRow>> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, SnapshotRow>(&format!(
        "UPDATE machine_snapshots SET state = 'ready', consistency = $2, manifest = $3, \
             size_bytes = $4, stored_bytes = $5, error = NULL, completed_at = now() \
         WHERE id = $1 AND state IN ('pending', 'uploading') RETURNING {SNAPSHOT_COLS}"
    ))
    .bind(id)
    .bind(r.consistency)
    .bind(r.manifest)
    .bind(r.size_bytes)
    .bind(r.stored_bytes)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(s) = &row {
        refresh_latest(&mut tx, s.machine_id).await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// Point a machine at its newest ready snapshot, or at none.
async fn refresh_latest(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, machine_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE machines SET latest_snapshot_id = ( \
             SELECT id FROM machine_snapshots \
             WHERE machine_id = $1 AND state = 'ready' ORDER BY created_at DESC LIMIT 1) \
         WHERE id = $1",
    )
    .bind(machine_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Queue one snapshot for deletion. False when it was open or already gone.
pub async fn mark_deleting(pool: &PgPool, id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let machine: Option<Uuid> = sqlx::query_scalar(
        "UPDATE machine_snapshots SET state = 'deleting' \
         WHERE id = $1 AND state IN ('ready', 'skipped', 'failed') RETURNING machine_id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(m) = machine {
        refresh_latest(&mut tx, m).await?;
    }
    tx.commit().await?;
    Ok(machine.is_some())
}

/// Queue every snapshot of a machine for deletion, open ones included: a
/// purge must not leave a capture finishing into a bucket nobody tracks.
pub async fn mark_all_deleting(pool: &PgPool, machine_id: Uuid) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "UPDATE machine_snapshots SET state = 'deleting' \
         WHERE machine_id = $1 AND state NOT IN ('deleting', 'deleted')",
    )
    .bind(machine_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    refresh_latest(&mut tx, machine_id).await?;
    tx.commit().await?;
    Ok(n)
}

/// Captures still open long after they were ordered.
pub async fn stale_open(pool: &PgPool, older_than_s: i64) -> Result<Vec<SnapshotRow>> {
    Ok(sqlx::query_as::<_, SnapshotRow>(&format!(
        "SELECT {SNAPSHOT_COLS} FROM machine_snapshots \
         WHERE state IN ('pending', 'uploading') AND created_at < now() - make_interval(secs => $1) \
         LIMIT 100"
    ))
    .bind(older_than_s as f64)
    .fetch_all(pool)
    .await?)
}

/// Ready snapshots past their machine's `keep`. Pinned ones do not count
/// and are never picked, and each machine keeps its newest clean one however
/// many live ones came after it.
pub async fn over_retention(pool: &PgPool, default_keep: i32) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "WITH ranked AS ( \
             SELECT s.id, s.consistency, \
                    row_number() OVER (PARTITION BY s.machine_id ORDER BY s.created_at DESC) AS n, \
                    row_number() OVER (PARTITION BY s.machine_id, s.consistency \
                                       ORDER BY s.created_at DESC) AS nc, \
                    GREATEST(COALESCE((m.snapshot_policy->>'keep')::int, $1), 1) AS keep \
             FROM machine_snapshots s JOIN machines m ON m.id = s.machine_id \
             WHERE s.state = 'ready' AND NOT s.pinned) \
         SELECT id FROM ranked \
         WHERE n > keep AND NOT (consistency = 'clean' AND nc = 1) LIMIT 200",
    )
    .bind(default_keep)
    .fetch_all(pool)
    .await?)
}

/// Snapshots of machines destroyed longer ago than the retention window.
pub async fn of_destroyed_machines(pool: &PgPool, days: i64) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT s.id FROM machine_snapshots s JOIN machines m ON m.id = s.machine_id \
         WHERE m.state = 'destroyed' AND m.destroyed_at < now() - make_interval(days => $1) \
           AND s.state IN ('ready', 'skipped', 'failed') LIMIT 200",
    )
    .bind(days as i32)
    .fetch_all(pool)
    .await?)
}

/// Failed and skipped snapshots have told their story after a day.
pub async fn expire_leftovers(pool: &PgPool) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE machine_snapshots SET state = 'deleting' \
         WHERE state IN ('failed', 'skipped') AND created_at < now() - interval '1 day'",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn deleting(pool: &PgPool, limit: i64) -> Result<Vec<SnapshotRow>> {
    Ok(sqlx::query_as::<_, SnapshotRow>(&format!(
        "SELECT {SNAPSHOT_COLS} FROM machine_snapshots WHERE state = 'deleting' LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Whether any snapshot other than `except`, and not itself on its way out,
/// still names this object.
pub async fn key_in_use(pool: &PgPool, key: &str, except: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM machine_snapshot_objects o \
             JOIN machine_snapshots s ON s.id = o.snapshot_id \
             WHERE o.key = $1 AND o.snapshot_id <> $2 AND o.state <> 'deleted' \
               AND s.state NOT IN ('deleting', 'deleted'))",
    )
    .bind(key)
    .bind(except)
    .fetch_one(pool)
    .await?)
}

pub async fn object_deleted(pool: &PgPool, snapshot_id: Uuid, layer: &str) -> Result<()> {
    sqlx::query("UPDATE machine_snapshot_objects SET state = 'deleted' WHERE snapshot_id = $1 AND layer = $2")
        .bind(snapshot_id)
        .bind(layer)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn finish_delete(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE machine_snapshots SET state = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
