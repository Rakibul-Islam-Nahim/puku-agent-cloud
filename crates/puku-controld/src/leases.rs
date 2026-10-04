//! controld-side lease wrapper.
//!
//! Per docs/RELIABILITY-REBUILD.md §5.2. Wraps `puku-leases::LeaseService`
//! with controld-specific config (1 s tick, BMC probe policy).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::PgPool;
use uuid::Uuid;

use puku_leases::{
    BmcEndpoint, Lease, LeaseError, LeaseService, LeaseServiceImpl, LeaseState, LeaseStore,
};

/// Postgres-backed lease store. One row per host.
pub struct PgLeaseStore {
    pub pool: PgPool,
}

fn lease_from_row(r: &sqlx::postgres::PgRow) -> Lease {
    let state_str: String = r.get("state");
    let state = match state_str.as_str() {
        "held" => LeaseState::Held,
        "suspected" => LeaseState::Suspected,
        _ => LeaseState::Released,
    };
    let bmc_hostname: Option<String> = r.try_get("bmc_hostname").ok();
    let bmc_kind: Option<String> = r.try_get("bmc_kind").ok();
    let bmc = bmc_hostname.map(|h| BmcEndpoint {
        hostname: h,
        kind: match bmc_kind.as_deref() {
            Some("ipmi") => puku_leases::BmcKind::Ipmi,
            Some("redfish") => puku_leases::BmcKind::Redfish,
            _ => puku_leases::BmcKind::Ipmi,
        },
        username: String::new(),
        password: String::new(),
    });
    Lease {
        host_id: r.get("host_id"),
        generation: r.get::<i64, _>("generation") as u64,
        state,
        expires_at: DateTime::<Utc>::from(r.get::<chrono::DateTime<Utc>, _>("expires_at")),
        last_renewed_at: DateTime::<Utc>::from(r.get::<chrono::DateTime<Utc>, _>("last_renewed_at")),
        bmc,
        owner_instance: r.get("owner_instance"),
    }
}

#[async_trait::async_trait]
impl LeaseStore for PgLeaseStore {
    async fn load(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError> {
        let row = sqlx::query(
            "SELECT host_id, generation, owner_instance, expires_at, last_renewed_at, bmc_hostname, bmc_kind, state \
             FROM leases WHERE host_id = $1",
        )
        .bind(host_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| LeaseError::Internal(e.to_string()))?;
        Ok(row.as_ref().map(lease_from_row))
    }

    async fn upsert(&self, lease: &Lease) -> Result<(), LeaseError> {
        let bmc_kind_str: Option<&str> = lease.bmc.as_ref().map(|b| match b.kind {
            puku_leases::BmcKind::Ipmi => "ipmi",
            puku_leases::BmcKind::Redfish => "redfish",
        });
        sqlx::query(
            "INSERT INTO leases (host_id, owner_instance, generation, acquired_at, expires_at, last_renewed_at, state, bmc_hostname, bmc_kind) \
             VALUES ($1, $2, $3, now(), $4, $5, $6, $7, $8) \
             ON CONFLICT (host_id) DO UPDATE SET \
               owner_instance = EXCLUDED.owner_instance, \
               generation = EXCLUDED.generation, \
               expires_at = EXCLUDED.expires_at, \
               last_renewed_at = EXCLUDED.last_renewed_at, \
               state = EXCLUDED.state, \
               bmc_hostname = EXCLUDED.bmc_hostname, \
               bmc_kind = EXCLUDED.bmc_kind",
        )
        .bind(lease.host_id)
        .bind(&lease.owner_instance)
        .bind(lease.generation as i64)
        .bind(lease.expires_at)
        .bind(lease.last_renewed_at)
        .bind(lease.state.as_str())
        .bind(lease.bmc.as_ref().map(|b| b.hostname.clone()))
        .bind(bmc_kind_str)
        .execute(&self.pool)
        .await
        .map_err(|e| LeaseError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_expiring(&self, before: DateTime<Utc>) -> Result<Vec<Lease>, LeaseError> {
        let rows = sqlx::query(
            "SELECT host_id, generation, owner_instance, expires_at, last_renewed_at, bmc_hostname, bmc_kind, state \
             FROM leases WHERE state = 'held' AND expires_at <= $1",
        )
        .bind(before)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| LeaseError::Internal(e.to_string()))?;
        Ok(rows.iter().map(lease_from_row).collect())
    }

    async fn list_suspected(&self) -> Result<Vec<Lease>, LeaseError> {
        let rows = sqlx::query(
            "SELECT host_id, generation, owner_instance, expires_at, last_renewed_at, bmc_hostname, bmc_kind, state \
             FROM leases WHERE state = 'suspected'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| LeaseError::Internal(e.to_string()))?;
        Ok(rows.iter().map(lease_from_row).collect())
    }

    async fn delete(&self, host_id: Uuid) -> Result<(), LeaseError> {
        sqlx::query("DELETE FROM leases WHERE host_id = $1")
            .bind(host_id)
            .execute(&self.pool)
            .await
            .map_err(|e| LeaseError::Internal(e.to_string()))?;
        Ok(())
    }
}

/// Build the controld lease service for a given instance id.
pub fn build_service(pool: PgPool, instance: &str) -> Arc<dyn LeaseService> {
    let store = Arc::new(PgLeaseStore { pool });
    Arc::new(LeaseServiceImpl::new(store, instance))
}

/// Spawn the sweeper that runs every 1 s.
pub fn start_sweeper(svc: Arc<dyn LeaseService>, store: Arc<PgLeaseStore>) {
    use puku_leases::{BmcProbe, LeaseSweeper};
    let bmc: Arc<dyn BmcProbe> = Arc::new(puku_leases::bmc_probe::BmcProbeStub);
    let store_dyn: Arc<dyn LeaseStore> = store;
    let sweeper = LeaseSweeper::new(store_dyn, svc, bmc);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            if let Err(e) = sweeper.sweep_once().await {
                tracing::warn!(error = %e, "lease sweeper error");
            }
        }
    });
}

/// `handle_lease_lost(host_id)` triggers the fence. Per RSD §5.2.
pub async fn handle_lease_lost(host_id: Uuid) {
    tracing::info!(host_id = %host_id, "lease lost; fence scheduled");
    // Fence is delegated to the fence module. Kept here as a thin wrapper
    // so callers don't need to know the wiring.
    crate::fence::fence_on_lease_lost(host_id).await;
}

/// Public API used by scheduler.
pub async fn is_host_healthy(svc: &dyn LeaseService, host_id: Uuid) -> bool {
    match svc.lookup(host_id).await {
        Ok(Some(l)) => l.state == LeaseState::Held && !l.is_expired(chrono::Utc::now()),
        _ => false,
    }
}