//! Event archival: terminal sessions past the retention window get their
//! event log compacted to one ndjson file under the archive dir, their rows
//! deleted from Postgres, and their state set to `reaped`. Keeps the hot
//! event table bounded.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::{db, AppState};

/// Re-entry tier transition: when a session with `tier='cold_archived'` is
/// resumed, bump it back to `hibernated` so the sweeper picks it up. RSD §5.4.2.
/// Returns Ok(true) when the row was actually transitioned.
pub async fn tier_warm_on_resume(state: &AppState, session_id: Uuid) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE sessions SET tier = 'hibernated', tier_changed_at = now(), last_resumed_at = now() \
         WHERE id = $1 AND tier IN ('cold_archived', 'archived') \
         RETURNING id",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(res.is_some())
}

/// Tier transition: running -> hibernated when the session is parked. Called
/// from the session-state-transition path (api/post_stop). RSD §5.4.2.
pub async fn tier_to_hibernated(state: &AppState, session_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE sessions SET tier = 'hibernated', tier_changed_at = now() \
         WHERE id = $1 AND tier = 'warm'",
    )
    .bind(session_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

pub fn spawn(state: AppState, archive_dir: PathBuf, retention_days: i64) {
    tokio::spawn(async move {
        // First pass shortly after boot, then every 6 hours.
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if let Err(e) = run_once(&state, &archive_dir, retention_days).await {
                tracing::warn!(error = format!("{e:#}"), "archive pass failed");
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

async fn run_once(state: &AppState, archive_dir: &PathBuf, retention_days: i64) -> Result<()> {
    std::fs::create_dir_all(archive_dir)?;
    // worker_id comes along so the volumes can be reaped too: archiving the
    // transcript is only half of it, and the half that was missing left every
    // session's workspace and unpacked skill packs on the worker's disk
    // forever.
    let candidates: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, worker_id FROM sessions \
         WHERE state IN ('completed','failed','canceled') \
           AND archived_at IS NULL \
           AND ended_at < now() - make_interval(days => $1::int) \
         LIMIT 50",
    )
    .bind(retention_days as i32)
    .fetch_all(&state.pool)
    .await?;

    for (session_id, worker_id) in candidates {
        // Memory before archival, or the transcript is gone and the memory
        // with it.
        //
        // `spawn_ingest` is detached, so a controld restart between the
        // terminal transition and the POST leaves `memory_ingested_at` NULL
        // with the events still present. That is recoverable -- until this
        // job deletes the rows. Draining here is the last chance, and it is
        // cheap: on a healthy deployment every candidate was ingested weeks
        // ago and this does nothing.
        if let Err(e) = crate::memory::ingest_before_archive(state, session_id).await {
            tracing::warn!(%session_id, error = format!("{e:#}"),
                "pre-archive memory ingest failed; archiving anyway");
        }

        let path = archive_dir.join(format!("{session_id}.ndjson"));
        let mut out = String::new();
        let mut cursor = 0i64;
        loop {
            let batch = db::fetch_events(&state.pool, session_id, cursor, 1000).await?;
            if batch.is_empty() {
                break;
            }
            cursor = batch.last().unwrap().seq;
            for ev in &batch {
                out.push_str(&serde_json::to_string(ev)?);
                out.push('\n');
            }
        }
        // Object storage is the durable home; the local file is the
        // fallback for a deployment without R2. Never delete the rows
        // before the archive is safely written, or the transcript is gone.
        let destination = match &state.blobs {
            Some(blobs) => {
                let key = crate::blobstore::BlobStore::archive_key(session_id);
                blobs
                    .put(&key, out.into_bytes(), "application/x-ndjson")
                    .await
                    .with_context(|| format!("archiving {session_id} to object storage"))?;
                blobs.blob_ref(&key)
            }
            None => {
                std::fs::write(&path, out)?;
                path.display().to_string()
            }
        };

        // Direct update, not transition(): archival must not append new
        // event rows to the table it just emptied.
        let mut tx = state.pool.begin().await?;
        sqlx::query("DELETE FROM session_events WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE sessions SET archived_at = now(), state = 'reaped' WHERE id = $1",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // Now that the transcript is safe and the rows are gone, tell the
        // worker it can drop the volumes. Best-effort on purpose: a worker
        // that is offline simply keeps the directory until someone sweeps it,
        // which is strictly better than blocking archival on its liveness.
        if worker_id.is_some() {
            let _ = crate::workerlink::send_to_worker(
                state,
                worker_id,
                puku_cloud_proto::worker_proto::Down::ReapSession { session_id },
            );
        }
        tracing::info!(%session_id, %destination, "session archived");
    }
    Ok(())
}
