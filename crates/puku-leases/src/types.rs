//! Lease service types and trait.
//!
//! A lease says "this worker host is alive". It belongs to the host and is
//! kept alive by the controld instance that holds the host's link: the
//! worker sends a renew frame every second, controld renews the row. When
//! the frames stop the lease runs out, the sweeper marks the host
//! *suspected* (no new work), and only after a further grace period
//! *dead* (recover its sessions elsewhere, after fencing).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// How long a lease lasts after the last renewal. Workers renew every 1 s,
/// so a silent host is suspected within ~3 s.
pub const LEASE_TTL_SECONDS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    /// Renewing on time.
    Held,
    /// Missed its renewals. Gets no new work; may still come back.
    Suspected,
    /// Declared dead. Its sessions are recovered elsewhere; the host must
    /// re-register (new generation) before it runs anything again.
    Released,
}

impl LeaseState {
    /// String form used when binding into sqlx queries (must match the
    /// values in the `leases.state` CHECK constraint from migration 0028).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Suspected => "suspected",
            Self::Released => "released",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "held" => Self::Held,
            "suspected" => Self::Suspected,
            _ => Self::Released,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BmcKind {
    Ipmi,
    Redfish,
}

#[derive(Debug, Clone)]
pub struct BmcEndpoint {
    pub hostname: String,
    pub kind: BmcKind,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub host_id: Uuid,
    /// Bumped on every takeover (each registration), never on renewal, so
    /// it names one continuous ownership and can fence a stale holder.
    pub generation: u64,
    pub state: LeaseState,
    pub expires_at: DateTime<Utc>,
    pub last_renewed_at: DateTime<Utc>,
    pub suspected_at: Option<DateTime<Utc>>,
    pub confirmed_dead_at: Option<DateTime<Utc>>,
    pub bmc: Option<BmcEndpoint>,
    /// The controld instance holding the host's link.
    pub owner_instance: String,
}

impl Lease {
    pub fn fresh(host_id: Uuid, owner_instance: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            host_id,
            generation: 1,
            state: LeaseState::Held,
            expires_at: now + Duration::seconds(LEASE_TTL_SECONDS),
            last_renewed_at: now,
            suspected_at: None,
            confirmed_dead_at: None,
            bmc: None,
            owner_instance: owner_instance.into(),
        }
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("lease not held by this instance")]
    NotHeld,
    #[error("lease not found for host {0}")]
    NotFound(Uuid),
    #[error("lease already suspected")]
    AlreadySuspected,
    #[error("host {0} was declared dead; it must re-register")]
    Dead(Uuid),
    #[error("internal: {0}")]
    Internal(String),
}

/// Backend abstraction. Production uses Postgres; tests use an in-memory map.
#[async_trait]
pub trait LeaseStore: Send + Sync {
    async fn load(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError>;
    async fn upsert(&self, lease: &Lease) -> Result<(), LeaseError>;
    async fn list_expiring(&self, before: DateTime<Utc>) -> Result<Vec<Lease>, LeaseError>;
    async fn list_suspected(&self) -> Result<Vec<Lease>, LeaseError>;
    /// Hosts that are held or suspected (the denominator of the mass-loss
    /// guard).
    async fn count_live(&self) -> Result<usize, LeaseError>;
    async fn delete(&self, host_id: Uuid) -> Result<(), LeaseError>;
}

#[async_trait]
pub trait LeaseService: Send + Sync {
    /// Acquire a lease. Fails if another controld instance holds a live one.
    async fn acquire(&self, host_id: Uuid, bmc: Option<BmcEndpoint>, owner: &str) -> Result<Lease, LeaseError>;

    /// The host registered on `owner`'s link: it is alive and talking to us,
    /// whatever the old row says. Starts a new generation.
    async fn takeover(&self, host_id: Uuid, owner: &str) -> Result<Lease, LeaseError>;

    /// Renew. Keeps the generation. A suspected host that renews is healthy
    /// again; a host declared dead cannot renew (it must take over).
    async fn renew(&self, lease: &Lease) -> Result<Lease, LeaseError>;

    /// Release. Call on clean shutdown.
    async fn release(&self, lease: &Lease) -> Result<(), LeaseError>;

    /// Look up current state of `host_id`.
    async fn lookup(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError>;

    /// Mark a lease suspected (control plane only).
    async fn mark_suspected(&self, host_id: Uuid) -> Result<(), LeaseError>;

    /// The link to the host dropped: expire the lease now so the sweeper
    /// suspects it on its next tick rather than after the TTL. A no-op when
    /// the host has meanwhile re-registered elsewhere (newer generation).
    async fn expire_now(&self, lease: &Lease) -> Result<(), LeaseError>;

    /// Declare the host dead (control plane only, after the grace period).
    async fn mark_dead(&self, host_id: Uuid) -> Result<(), LeaseError>;
}

/// `LeaseService` over any `LeaseStore`.
pub struct LeaseServiceImpl {
    pub store: Arc<dyn LeaseStore>,
    pub owner: String,
}

impl LeaseServiceImpl {
    pub fn new(store: Arc<dyn LeaseStore>, owner: impl Into<String>) -> Self {
        Self { store, owner: owner.into() }
    }
}

#[async_trait]
impl LeaseService for LeaseServiceImpl {
    async fn acquire(&self, host_id: Uuid, bmc: Option<BmcEndpoint>, owner: &str) -> Result<Lease, LeaseError> {
        if let Some(existing) = self.store.load(host_id).await? {
            let live = existing.state == LeaseState::Held && !existing.is_expired(Utc::now());
            if existing.owner_instance == owner && live {
                return Ok(existing);
            }
            if existing.owner_instance != owner && live {
                return Err(LeaseError::NotHeld);
            }
        }
        let mut lease = Lease::fresh(host_id, owner);
        lease.bmc = bmc;
        self.store.upsert(&lease).await?;
        Ok(lease)
    }

    async fn takeover(&self, host_id: Uuid, owner: &str) -> Result<Lease, LeaseError> {
        let prev = self.store.load(host_id).await?;
        let mut lease = Lease::fresh(host_id, owner);
        if let Some(p) = prev {
            lease.generation = p.generation + 1;
            lease.bmc = p.bmc;
        }
        self.store.upsert(&lease).await?;
        Ok(lease)
    }

    async fn renew(&self, lease: &Lease) -> Result<Lease, LeaseError> {
        let cur = self.store.load(lease.host_id).await?.ok_or(LeaseError::NotFound(lease.host_id))?;
        if cur.state == LeaseState::Released {
            return Err(LeaseError::Dead(lease.host_id));
        }
        if cur.owner_instance != lease.owner_instance || cur.generation != lease.generation {
            return Err(LeaseError::NotHeld);
        }
        let now = Utc::now();
        let mut next = cur;
        next.state = LeaseState::Held;
        next.suspected_at = None;
        next.expires_at = now + Duration::seconds(LEASE_TTL_SECONDS);
        next.last_renewed_at = now;
        self.store.upsert(&next).await?;
        Ok(next)
    }

    async fn release(&self, lease: &Lease) -> Result<(), LeaseError> {
        if let Some(cur) = self.store.load(lease.host_id).await? {
            if cur.state == LeaseState::Held && cur.owner_instance == lease.owner_instance {
                self.store.delete(lease.host_id).await?;
            }
        }
        Ok(())
    }

    async fn lookup(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError> {
        self.store.load(host_id).await
    }

    async fn mark_suspected(&self, host_id: Uuid) -> Result<(), LeaseError> {
        let mut cur = self.store.load(host_id).await?.ok_or(LeaseError::NotFound(host_id))?;
        if cur.state != LeaseState::Held {
            return Err(LeaseError::AlreadySuspected);
        }
        cur.state = LeaseState::Suspected;
        cur.suspected_at = Some(Utc::now());
        self.store.upsert(&cur).await
    }

    async fn expire_now(&self, lease: &Lease) -> Result<(), LeaseError> {
        if let Some(mut cur) = self.store.load(lease.host_id).await? {
            if cur.state == LeaseState::Held && cur.generation == lease.generation {
                cur.expires_at = Utc::now();
                self.store.upsert(&cur).await?;
            }
        }
        Ok(())
    }

    async fn mark_dead(&self, host_id: Uuid) -> Result<(), LeaseError> {
        let mut cur = self.store.load(host_id).await?.ok_or(LeaseError::NotFound(host_id))?;
        cur.state = LeaseState::Released;
        cur.confirmed_dead_at = Some(Utc::now());
        self.store.upsert(&cur).await
    }
}

/// In-memory `LeaseStore` for tests.
pub struct InMemoryLeaseStore {
    inner: tokio::sync::Mutex<std::collections::HashMap<Uuid, Lease>>,
}

impl InMemoryLeaseStore {
    pub fn new() -> Self {
        Self { inner: tokio::sync::Mutex::new(std::collections::HashMap::new()) }
    }
}

impl Default for InMemoryLeaseStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LeaseStore for InMemoryLeaseStore {
    async fn load(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError> {
        Ok(self.inner.lock().await.get(&host_id).cloned())
    }
    async fn upsert(&self, lease: &Lease) -> Result<(), LeaseError> {
        self.inner.lock().await.insert(lease.host_id, lease.clone());
        Ok(())
    }
    async fn list_expiring(&self, before: DateTime<Utc>) -> Result<Vec<Lease>, LeaseError> {
        Ok(self.inner.lock().await.values().filter(|l| l.state == LeaseState::Held && l.expires_at <= before).cloned().collect())
    }
    async fn list_suspected(&self) -> Result<Vec<Lease>, LeaseError> {
        Ok(self.inner.lock().await.values().filter(|l| l.state == LeaseState::Suspected).cloned().collect())
    }
    async fn count_live(&self) -> Result<usize, LeaseError> {
        Ok(self.inner.lock().await.values().filter(|l| l.state != LeaseState::Released).count())
    }
    async fn delete(&self, host_id: Uuid) -> Result<(), LeaseError> {
        self.inner.lock().await.remove(&host_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> (Arc<InMemoryLeaseStore>, LeaseServiceImpl) {
        let store = Arc::new(InMemoryLeaseStore::new());
        (store.clone(), LeaseServiceImpl::new(store, "instance-1"))
    }

    #[tokio::test]
    async fn acquire_then_renew_advances_expiry_and_keeps_generation() {
        let (_, svc) = svc();
        let l = svc.acquire(Uuid::new_v4(), None, "instance-1").await.unwrap();
        let next = svc.renew(&l).await.unwrap();
        assert!(next.expires_at >= l.expires_at);
        assert_eq!(next.generation, l.generation);
    }

    #[tokio::test]
    async fn different_owner_cannot_acquire_a_live_lease() {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc1 = LeaseServiceImpl::new(store.clone(), "instance-1");
        let svc2 = LeaseServiceImpl::new(store.clone(), "instance-2");
        let h = Uuid::new_v4();
        svc1.acquire(h, None, "instance-1").await.unwrap();
        assert!(matches!(svc2.acquire(h, None, "instance-2").await, Err(LeaseError::NotHeld)));
    }

    #[tokio::test]
    async fn takeover_bumps_generation_and_fences_the_old_holder() {
        let (_, svc) = svc();
        let h = Uuid::new_v4();
        let old = svc.takeover(h, "instance-1").await.unwrap();
        let new = svc.takeover(h, "instance-2").await.unwrap();
        assert_eq!(new.generation, old.generation + 1);
        // The previous holder's renewals are refused: wrong owner and generation.
        assert!(matches!(svc.renew(&old).await, Err(LeaseError::NotHeld)));
        assert!(svc.renew(&new).await.is_ok());
    }

    #[tokio::test]
    async fn a_suspected_host_that_renews_is_healthy_again() {
        let (store, svc) = svc();
        let h = Uuid::new_v4();
        let l = svc.takeover(h, "instance-1").await.unwrap();
        svc.mark_suspected(h).await.unwrap();
        assert!(store.load(h).await.unwrap().unwrap().suspected_at.is_some());
        let back = svc.renew(&l).await.unwrap();
        assert_eq!(back.state, LeaseState::Held);
        assert!(back.suspected_at.is_none());
    }

    #[tokio::test]
    async fn a_dead_host_cannot_renew_it_must_take_over() {
        let (_, svc) = svc();
        let h = Uuid::new_v4();
        let l = svc.takeover(h, "instance-1").await.unwrap();
        svc.mark_dead(h).await.unwrap();
        assert!(matches!(svc.renew(&l).await, Err(LeaseError::Dead(_))));
        let again = svc.takeover(h, "instance-1").await.unwrap();
        assert_eq!(again.generation, l.generation + 1);
        assert_eq!(again.state, LeaseState::Held);
    }

    #[tokio::test]
    async fn expire_now_makes_a_held_lease_expiring() {
        let (store, svc) = svc();
        let h = Uuid::new_v4();
        let l = svc.takeover(h, "instance-1").await.unwrap();
        svc.expire_now(&l).await.unwrap();
        assert_eq!(store.list_expiring(Utc::now()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stale_link_dropping_does_not_expire_the_new_one() {
        let (store, svc) = svc();
        let h = Uuid::new_v4();
        let old = svc.takeover(h, "instance-1").await.unwrap();
        svc.takeover(h, "instance-2").await.unwrap(); // reconnected elsewhere
        svc.expire_now(&old).await.unwrap(); // the old link finally closes
        assert!(store.list_expiring(Utc::now()).await.unwrap().is_empty());
    }
}
