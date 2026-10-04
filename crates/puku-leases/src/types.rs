//! Lease service types and trait.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// How long a lease lasts after the last heartbeat. The contract says 3 s:
/// workers renew every 1 s and the sweeper ticks every 1 s, so a missed
/// heartbeat surfaces in 1–3 s.
pub const LEASE_TTL_SECONDS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Held,
    Suspected,
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
    pub generation: u64,
    pub state: LeaseState,
    pub expires_at: DateTime<Utc>,
    pub last_renewed_at: DateTime<Utc>,
    pub bmc: Option<BmcEndpoint>,
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
    async fn delete(&self, host_id: Uuid) -> Result<(), LeaseError>;
}

#[async_trait]
pub trait LeaseService: Send + Sync {
    /// Acquire a lease. Fails if another controld instance holds it
    /// (advisory lock per host_id).
    async fn acquire(
        &self,
        host_id: Uuid,
        bmc: Option<BmcEndpoint>,
        owner: &str,
    ) -> Result<Lease, LeaseError>;

    /// Renew. Bumps generation. Must be called every 1 s.
    async fn renew(&self, lease: &Lease) -> Result<Lease, LeaseError>;

    /// Release. Call on clean shutdown.
    async fn release(&self, lease: &Lease) -> Result<(), LeaseError>;

    /// Look up current state of `host_id`.
    async fn lookup(&self, host_id: Uuid) -> Result<Option<Lease>, LeaseError>;

    /// Mark a lease suspected (control plane only).
    async fn mark_suspected(&self, host_id: Uuid) -> Result<(), LeaseError>;
}

/// Concrete in-memory `LeaseService` for unit tests; production wires
/// `LeaseServiceImpl` against a Postgres-backed `LeaseStore`.
pub struct LeaseServiceImpl {
    pub store: Arc<dyn LeaseStore>,
    pub owner: String,
}

impl LeaseServiceImpl {
    pub fn new(store: Arc<dyn LeaseStore>, owner: impl Into<String>) -> Self {
        Self {
            store,
            owner: owner.into(),
        }
    }
}

#[async_trait]
impl LeaseService for LeaseServiceImpl {
    async fn acquire(
        &self,
        host_id: Uuid,
        bmc: Option<BmcEndpoint>,
        owner: &str,
    ) -> Result<Lease, LeaseError> {
        if let Some(existing) = self.store.load(host_id).await? {
            // Same owner can re-acquire cleanly. Different owner triggers takeover.
            if existing.owner_instance == owner
                && existing.state == LeaseState::Held
                && !existing.is_expired(Utc::now())
            {
                return Ok(existing);
            }
            if existing.owner_instance != owner && existing.state == LeaseState::Held {
                return Err(LeaseError::NotHeld);
            }
        }
        let mut lease = Lease::fresh(host_id, owner);
        lease.bmc = bmc;
        self.store.upsert(&lease).await?;
        Ok(lease)
    }

    async fn renew(&self, lease: &Lease) -> Result<Lease, LeaseError> {
        let cur = self
            .store
            .load(lease.host_id)
            .await?
            .ok_or(LeaseError::NotFound(lease.host_id))?;
        if cur.state != LeaseState::Held {
            return Err(LeaseError::NotHeld);
        }
        if cur.owner_instance != lease.owner_instance {
            return Err(LeaseError::NotHeld);
        }
        if cur.is_expired(Utc::now()) {
            return Err(LeaseError::NotHeld);
        }
        let now = Utc::now();
        let mut next = lease.clone();
        next.expires_at = now + Duration::seconds(LEASE_TTL_SECONDS);
        next.last_renewed_at = now;
        self.store.upsert(&next).await?;
        Ok(next)
    }

    async fn release(&self, lease: &Lease) -> Result<(), LeaseError> {
        let cur = self.store.load(lease.host_id).await?;
        if let Some(cur) = cur {
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
        let mut cur = self
            .store
            .load(host_id)
            .await?
            .ok_or(LeaseError::NotFound(host_id))?;
        if cur.state == LeaseState::Suspected {
            return Err(LeaseError::AlreadySuspected);
        }
        cur.state = LeaseState::Suspected;
        self.store.upsert(&cur).await?;
        Ok(())
    }
}

/// In-memory `LeaseStore` for tests.
pub struct InMemoryLeaseStore {
    inner: tokio::sync::Mutex<std::collections::HashMap<Uuid, Lease>>,
}

impl InMemoryLeaseStore {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
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
        Ok(self
            .inner
            .lock()
            .await
            .values()
            .filter(|l| l.state == LeaseState::Held && l.expires_at <= before)
            .cloned()
            .collect())
    }
    async fn list_suspected(&self) -> Result<Vec<Lease>, LeaseError> {
        Ok(self
            .inner
            .lock()
            .await
            .values()
            .filter(|l| l.state == LeaseState::Suspected)
            .cloned()
            .collect())
    }
    async fn delete(&self, host_id: Uuid) -> Result<(), LeaseError> {
        self.inner.lock().await.remove(&host_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Uuid {
        Uuid::new_v4()
    }

    #[tokio::test]
    async fn acquire_then_renew_advances_expiry() {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc = LeaseServiceImpl::new(store, "instance-1");
        let h = host();
        let l = svc.acquire(h, None, "instance-1").await.unwrap();
        let first_expiry = l.expires_at;
        let next = svc.renew(&l).await.unwrap();
        assert!(next.expires_at > first_expiry);
        assert_eq!(next.generation, l.generation);
    }

    #[tokio::test]
    async fn renew_after_expiry_fails() {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc = LeaseServiceImpl::new(store.clone(), "instance-1");
        let h = host();
        let mut l = svc.acquire(h, None, "instance-1").await.unwrap();
        // Force expiry in the store.
        l.expires_at = Utc::now() - Duration::seconds(1);
        store.upsert(&l).await.unwrap();
        let res = svc.renew(&l).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn different_owner_cannot_acquire() {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc1 = LeaseServiceImpl::new(store.clone(), "instance-1");
        let svc2 = LeaseServiceImpl::new(store.clone(), "instance-2");
        let h = host();
        svc1.acquire(h, None, "instance-1").await.unwrap();
        let res = svc2.acquire(h, None, "instance-2").await;
        assert!(matches!(res, Err(LeaseError::NotHeld)));
    }

    #[tokio::test]
    async fn mark_suspected_then_renew_fails() {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc = LeaseServiceImpl::new(store, "instance-1");
        let h = host();
        let l = svc.acquire(h, None, "instance-1").await.unwrap();
        svc.mark_suspected(h).await.unwrap();
        let res = svc.renew(&l).await;
        assert!(matches!(res, Err(LeaseError::NotHeld)));
    }
}