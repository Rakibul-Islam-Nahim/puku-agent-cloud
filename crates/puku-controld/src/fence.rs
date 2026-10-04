//! controld-side fence wrapper.
//!
//! Per docs/RELIABILITY-REBUILD.md §5.3. Wraps `puku-fence::Fence` with
//! controld logging + audit (writes to `fence_log`).

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

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

/// Trigger a fence after the lease sweeper has marked a host `suspected`.
/// Per RSD §5.2 + §5.3.
pub async fn fence_on_lease_lost(host_id: Uuid) {
    // The fence itself needs the volume backend and the audit sink. In a
    // production wiring those are injected at startup; here we expose
    // `build_fencer` to construct one and call the actual fence from the
    // lease sweep orchestrator. This shim keeps the call site unchanged
    // when those are wired in later phases.
    tracing::info!(host_id = %host_id, "fence_on_lease_lost (no-op until Phase R3 wiring)");
}

/// Build the fencer for the controld instance. Wired by `start_recovery`.
pub fn build_fencer(
    volume: Arc<dyn puku_volume::VolumeBackend>,
    audit: Arc<PgAuditSink>,
    instance: &str,
) -> Arc<dyn Fence> {
    Arc::new(puku_fence::CephFencer::new(volume, audit, instance))
}