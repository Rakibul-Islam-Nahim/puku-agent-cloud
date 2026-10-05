//! controld-side fence wrapper.
//!
//! Per docs/RELIABILITY-REBUILD.md §5.3. Wraps `puku-fence::Fence` with
//! controld logging + audit (writes to `fence_log`).

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use puku_fence::{AuditEntry, AuditSink, Fence};

/// Postgres-backed audit sink. Writes to `fence_log` and returns the id.
pub struct PgAuditSink {
    pub pool: PgPool,
}

#[async_trait]
impl AuditSink for PgAuditSink {
    async fn write(&self, entry: &AuditEntry) -> i64 {
        let row: Result<Option<(i64,)>, _> = sqlx::query_as(
            "INSERT INTO fence_log (host_id, session_id, action, outcome, detail, requested_by) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(entry.host_id)
        .bind(entry.session_id)
        .bind(&entry.action)
        .bind(&entry.outcome)
        .bind(&entry.detail)
        .bind(&entry.requested_by)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "fence audit insert failed");
            e
        });
        row.ok().flatten().map(|r| r.0).unwrap_or(0)
    }
}

/// Build the fencer for the controld instance (shared session disks).
pub fn build_fencer(
    volume: Arc<dyn puku_volume::VolumeBackend>,
    audit: Arc<PgAuditSink>,
    instance: &str,
) -> Arc<dyn Fence> {
    Arc::new(puku_fence::CephFencer::new(volume, audit, instance))
}