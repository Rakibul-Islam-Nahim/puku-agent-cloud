//! controld-side lease wrapper.
//!
//! Per docs/RELIABILITY-REBUILD.md §5.2, with the review corrections. The
//! lease row for a worker host is taken over by whichever controld instance
//! holds that worker's link (`workerlink`), renewed on every
//! `Up::LeaseRenew` frame, and expired at once when the link drops. One
//! instance at a time -- whoever holds the Postgres advisory lock -- sweeps.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::PgPool;
use uuid::Uuid;

use puku_leases::{
    BmcEndpoint, Lease, LeaseError, LeaseService, LeaseServiceImpl, LeaseState, LeaseStore, LeaseSweeper, SweepReport,
};

/// Advisory-lock key for the sweeper leader ("pukuleas").
const SWEEPER_LOCK_KEY: i64 = 0x7075_6b75_6c65_6173;

const COLUMNS: &str = "host_id, generation, owner_instance, expires_at, last_renewed_at, suspected_at, \
                       confirmed_dead_at, bmc_hostname, bmc_kind, state";

/// Postgres-backed lease store. One row per host.
pub struct PgLeaseStore {
    pub pool: PgPool,
}

fn lease_from_row(r: &sqlx::postgres::PgRow) -> Lease {
    let state = LeaseState::parse(r.get::<String, _>("state").as_str());
    let bmc_hostname: Option<String> = r.try_get("bmc_hostname").ok().flatten();
    let bmc_kind: Option<String> = r.try_get("bmc_kind").ok().flatten();
    let bmc = bmc_hostname.map(|h| BmcEndpoint {
        hostname: h,
        kind: match bmc_kind.as_deref() {
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
        expires_at: r.get::<DateTime<Utc>, _>("expires_at"),
        last_renewed_at: r.get::<DateTime<Utc>, _>("last_renewed_at"),
        suspected_at: r.try_get("suspected_at").ok().flatten(),
        confirmed_dead_at: r.try_get("confirmed_dead_at").ok().flatten(),
        bmc,
        owner_instance: r.get("owner_instance"),
    }
}

fn internal(e: sqlx::Error) -> LeaseError {
    LeaseError::Internal(e.to_string())
}

#[async_trait::async_trait]
impl LeaseStore for PgLeaseStore {
    async fn load(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM leases WHERE host_id = $1"))
            .bind(host_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
        Ok(row.as_ref().map(lease_from_row))
    }

    async fn upsert(&self, lease: &Lease) -> Result<(), LeaseError> {
        let bmc_kind_str: Option<&str> = lease.bmc.as_ref().map(|b| match b.kind {
            puku_leases::BmcKind::Ipmi => "ipmi",
            puku_leases::BmcKind::Redfish => "redfish",
        });
        sqlx::query(
            "INSERT INTO leases (host_id, owner_instance, generation, acquired_at, expires_at, last_renewed_at, state, \
                                 bmc_hostname, bmc_kind, suspected_at, confirmed_dead_at) \
             VALUES ($1, $2, $3, now(), $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (host_id) DO UPDATE SET \
               owner_instance = EXCLUDED.owner_instance, \
               generation = EXCLUDED.generation, \
               acquired_at = CASE WHEN leases.generation = EXCLUDED.generation THEN leases.acquired_at ELSE now() END, \
               expires_at = EXCLUDED.expires_at, \
               last_renewed_at = EXCLUDED.last_renewed_at, \
               state = EXCLUDED.state, \
               bmc_hostname = EXCLUDED.bmc_hostname, \
               bmc_kind = EXCLUDED.bmc_kind, \
               suspected_at = EXCLUDED.suspected_at, \
               confirmed_dead_at = EXCLUDED.confirmed_dead_at",
        )
        .bind(lease.host_id)
        .bind(&lease.owner_instance)
        .bind(lease.generation as i64)
        .bind(lease.expires_at)
        .bind(lease.last_renewed_at)
        .bind(lease.state.as_str())
        .bind(lease.bmc.as_ref().map(|b| b.hostname.clone()))
        .bind(bmc_kind_str)
        .bind(lease.suspected_at)
        .bind(lease.confirmed_dead_at)
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn list_expiring(&self, before: DateTime<Utc>) -> Result<Vec<Lease>, LeaseError> {
        let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM leases WHERE state = 'held' AND expires_at <= $1"))
            .bind(before)
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(lease_from_row).collect())
    }

    async fn list_suspected(&self) -> Result<Vec<Lease>, LeaseError> {
        let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM leases WHERE state = 'suspected'"))
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(lease_from_row).collect())
    }

    async fn count_live(&self) -> Result<usize, LeaseError> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM leases WHERE state <> 'released'")
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        Ok(n as usize)
    }

    async fn delete(&self, host_id: Uuid) -> Result<(), LeaseError> {
        sqlx::query("DELETE FROM leases WHERE host_id = $1")
            .bind(host_id)
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
    }
}

/// Build the controld lease service for a given instance id.
pub fn build_service(pool: PgPool, instance: &str) -> Arc<dyn LeaseService> {
    let store = Arc::new(PgLeaseStore { pool });
    Arc::new(LeaseServiceImpl::new(store, instance))
}

/// The lease service as this controld instance: what `workerlink` uses to
/// take over, renew and expire the leases of the workers linked to it.
pub fn service_for(state: &crate::AppState) -> Arc<dyn LeaseService> {
    build_service(state.pool.clone(), &state.cfg.instance_id.to_string())
}

/// Spawn the sweeper. Every instance runs the loop; only the one holding
/// the advisory lock sweeps. If this instance dies or loses Postgres the
/// lock goes with its session and another instance takes over on its next
/// tick.
pub fn start_sweeper(state: crate::AppState, svc: Arc<dyn LeaseService>, store: Arc<PgLeaseStore>) {
    use puku_leases::BmcProbe;
    let pool = store.pool.clone();
    let bmc: Arc<dyn BmcProbe> = Arc::new(puku_leases::bmc_probe::BmcProbeStub);
    let store_dyn: Arc<dyn LeaseStore> = store;
    let sweeper = LeaseSweeper::new(store_dyn, svc, bmc);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut lock = SweepLock::new();
        let mut held_for_mass_loss = false;
        // Dead hosts whose settling failed (Postgres blip): retried every
        // tick, since the sweeper reports a host dead only once.
        let mut unsettled = std::collections::HashSet::new();
        loop {
            tick.tick().await;
            if !lock.hold(&pool).await {
                continue;
            }
            match sweeper.sweep_once().await {
                Ok(report) => act_on(&state, &report, &mut held_for_mass_loss, &mut unsettled).await,
                Err(e) => tracing::warn!(error = %e, "lease sweeper error"),
            }
        }
    });
}

/// The sweeper's leadership: a session-level advisory lock on a connection
/// of its own. The connection is detached from the pool, so dropping it
/// closes the session -- returning it to the pool would keep the lock
/// alive on an idle connection nobody sweeps with.
pub(crate) struct SweepLock {
    conn: Option<sqlx::PgConnection>,
    held: bool,
    key: i64,
}

impl SweepLock {
    pub(crate) fn new() -> Self {
        Self::with_key(SWEEPER_LOCK_KEY)
    }

    /// A leader lock for another single-instance job.
    pub(crate) fn with_key(key: i64) -> Self {
        Self { conn: None, held: false, key }
    }

    /// Keep the lock, or try to take it. Returns whether we hold it.
    pub(crate) async fn hold(&mut self, pool: &PgPool) -> bool {
        if self.conn.is_none() {
            match pool.acquire().await {
                Ok(c) => self.conn = Some(c.detach()),
                Err(_) => return false,
            }
        }
        let conn = self.conn.as_mut().expect("connection just set");
        let res = if self.held {
            // The lock is only as alive as the session holding it.
            sqlx::query("SELECT 1").execute(&mut *conn).await.map(|_| true)
        } else {
            sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
                .bind(self.key)
                .fetch_one(&mut *conn)
                .await
        };
        match res {
            Ok(now_held) => {
                if now_held && !self.held {
                    tracing::info!("this instance is now the lease sweeper");
                }
                self.held = now_held;
                now_held
            }
            Err(e) => {
                if self.held {
                    tracing::warn!(error = %e, "lease sweeper lost its lock connection");
                }
                self.conn = None;
                self.held = false;
                false
            }
        }
    }
}

async fn act_on(
    state: &crate::AppState,
    report: &SweepReport,
    held_for_mass_loss: &mut bool,
    unsettled: &mut std::collections::HashSet<Uuid>,
) {
    for host in &report.newly_suspected {
        tracing::warn!(host_id = %host, "worker host missed its lease; suspected, no new work");
    }
    match &report.mass_loss_hold {
        Some(m) => {
            if !*held_for_mass_loss {
                // Paging is the log alert on this line; nothing is fenced
                // or moved until it clears or an operator acts.
                tracing::error!(
                    suspected = m.suspected,
                    live = m.live,
                    "MASS HOST LOSS: holding every dead-host declaration; likely network or control-plane fault"
                );
            }
            *held_for_mass_loss = true;
        }
        None if *held_for_mass_loss => {
            tracing::info!("mass host loss cleared; sweeping normally again");
            *held_for_mass_loss = false;
        }
        None => {}
    }
    unsettled.extend(report.newly_dead.iter().copied());
    let due: Vec<Uuid> = unsettled.iter().copied().collect();
    for host in due {
        if handle_lease_lost(state, host).await {
            unsettled.remove(&host);
        }
    }
}

/// A host was declared dead: settle what it ran (see `hostloss`).
/// Per RSD §5.2. Returns false when it must be retried.
pub async fn handle_lease_lost(state: &crate::AppState, host_id: Uuid) -> bool {
    tracing::error!(host_id = %host_id, "worker host declared dead");
    match crate::hostloss::on_host_dead(state, host_id).await {
        Ok(_) => true,
        Err(e) => {
            tracing::error!(host_id = %host_id, error = format!("{e:#}"), "settling a dead host failed; retrying");
            false
        }
    }
}

/// Public API used by scheduler.
pub async fn is_host_healthy(svc: &dyn LeaseService, host_id: Uuid) -> bool {
    match svc.lookup(host_id).await {
        Ok(Some(l)) => l.state == LeaseState::Held && !l.is_expired(chrono::Utc::now()),
        _ => false,
    }
}
