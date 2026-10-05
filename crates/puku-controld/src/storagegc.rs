//! Storage cleanup for shared (RBD) disks, run by controld.
//!
//! Workers delete a disk when they are told to -- a reaped session, a
//! destroyed machine -- but only a worker that is up hears that. A disk
//! whose host was down at the time stayed in the pool for ever, and enough
//! of them fill it. This sweep needs no worker: it lists the pool, and
//! deletes every image whose session or machine is finished for good.
//!
//! Rules, all of which must hold before an image is deleted:
//!
//! - **It is ours and it is finished.** A session image (`<session id>`)
//!   whose session is archived or reaped, or no longer exists; a machine
//!   image (`machine-<id>`) whose machine is destroyed, or no longer
//!   exists. Any other name is never touched.
//! - **It has looked finished for the whole grace period** (default 1 h),
//!   across sweeps. A single sweep never deletes anything it has not seen
//!   finished before, so a row a moment from being written cannot lose its
//!   disk.
//! - **Nobody has it open.** An image with watchers is skipped, even
//!   finished: something still maps it, and deleting under it is how data
//!   is lost. (Ceph refuses that delete anyway.)
//!
//! Every deletion lands in the audit log. `dry_run` logs what it would
//! delete and deletes nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use uuid::Uuid;

/// The Ceph operations controld runs itself (storage cleanup, disk
/// backups), so tests can stand in for Ceph. Images are named within the
/// shared pool.
#[async_trait]
pub trait PoolAdmin: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<String>>;
    async fn watchers(&self, image: &str) -> anyhow::Result<Vec<String>>;
    async fn remove(&self, image: &str) -> anyhow::Result<()>;
    async fn exists(&self, image: &str) -> anyhow::Result<bool>;
    async fn snap_create(&self, image: &str, snap: &str) -> anyhow::Result<()>;
    async fn snap_remove(&self, image: &str, snap: &str) -> anyhow::Result<()>;
    async fn snap_list(&self, image: &str) -> anyhow::Result<Vec<String>>;
    async fn changed_since(&self, image: &str, from: &str, to: &str) -> anyhow::Result<bool>;
    async fn export_full(&self, image: &str, snap: &str, path: &str) -> anyhow::Result<()>;
    async fn export_diff(&self, image: &str, from: &str, to: &str, path: &str) -> anyhow::Result<()>;
    async fn import_full(&self, path: &str, image: &str, snap: &str) -> anyhow::Result<()>;
    async fn import_diff(&self, path: &str, image: &str) -> anyhow::Result<()>;
}

/// `PoolAdmin` over the real RBD backend.
pub struct RbdPoolAdmin {
    pub backend: puku_volume::RbdBackend,
}

#[async_trait]
impl PoolAdmin for RbdPoolAdmin {
    async fn list(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.backend.list_images().await?)
    }
    async fn watchers(&self, image: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.backend.watchers(&self.backend.image(image)?).await?)
    }
    async fn remove(&self, image: &str) -> anyhow::Result<()> {
        Ok(self.backend.remove(&self.backend.image(image)?).await?)
    }
    async fn exists(&self, image: &str) -> anyhow::Result<bool> {
        Ok(self.backend.exists(&self.backend.image(image)?).await?)
    }
    async fn snap_create(&self, image: &str, snap: &str) -> anyhow::Result<()> {
        Ok(self.backend.snap_create(&self.backend.image(image)?, snap).await?)
    }
    async fn snap_remove(&self, image: &str, snap: &str) -> anyhow::Result<()> {
        Ok(self.backend.snap_remove(&self.backend.image(image)?, snap).await?)
    }
    async fn snap_list(&self, image: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.backend.snap_list(&self.backend.image(image)?).await?)
    }
    async fn changed_since(&self, image: &str, from: &str, to: &str) -> anyhow::Result<bool> {
        Ok(self.backend.changed_since(&self.backend.image(image)?, from, to).await?)
    }
    async fn export_full(&self, image: &str, snap: &str, path: &str) -> anyhow::Result<()> {
        Ok(self.backend.export_full(&self.backend.image(image)?, snap, path).await?)
    }
    async fn export_diff(&self, image: &str, from: &str, to: &str, path: &str) -> anyhow::Result<()> {
        Ok(self.backend.export_diff(&self.backend.image(image)?, from, to, path).await?)
    }
    async fn import_full(&self, path: &str, image: &str, snap: &str) -> anyhow::Result<()> {
        Ok(self.backend.import_full(path, &self.backend.image(image)?, snap).await?)
    }
    async fn import_diff(&self, path: &str, image: &str) -> anyhow::Result<()> {
        Ok(self.backend.import_diff(path, &self.backend.image(image)?).await?)
    }
}

#[derive(Debug, Clone)]
pub struct GcPolicy {
    pub interval: Duration,
    pub grace: Duration,
    pub dry_run: bool,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self { interval: Duration::from_secs(600), grace: Duration::from_secs(3600), dry_run: false }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GcReport {
    pub deleted: Vec<String>,
    /// Finished, but not yet for the whole grace period.
    pub waiting: Vec<String>,
    /// Finished, but something still has them open.
    pub open: Vec<String>,
    /// Would have been deleted (dry run).
    pub would_delete: Vec<String>,
}

/// Why an image is garbage, if it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    Garbage(&'static str),
}

pub struct StorageGc {
    pub admin: Arc<dyn PoolAdmin>,
    pub policy: GcPolicy,
    /// When each image was first seen finished. In memory: a new leader
    /// starts the grace over, which only ever delays a delete.
    seen: Mutex<HashMap<String, Instant>>,
}

impl StorageGc {
    pub fn new(admin: Arc<dyn PoolAdmin>, policy: GcPolicy) -> Self {
        Self { admin, policy, seen: Mutex::new(HashMap::new()) }
    }

    pub async fn sweep_once(&self, pool: &sqlx::PgPool) -> anyhow::Result<GcReport> {
        let mut report = GcReport::default();
        let images = self.admin.list().await?;
        let now = Instant::now();
        let mut still_garbage = Vec::new();
        for image in images {
            let why = match verdict(pool, &image).await? {
                Verdict::Keep => continue,
                Verdict::Garbage(why) => why,
            };
            still_garbage.push(image.clone());
            let first = *self.seen.lock().unwrap().entry(image.clone()).or_insert(now);
            if now.duration_since(first) < self.policy.grace {
                report.waiting.push(image);
                continue;
            }
            let watchers = self.admin.watchers(&image).await?;
            if !watchers.is_empty() {
                tracing::warn!(%image, ?watchers, "finished disk is still open somewhere; not deleting it");
                report.open.push(image);
                continue;
            }
            if self.policy.dry_run {
                tracing::info!(%image, why, "storage cleanup (dry run): would delete");
                report.would_delete.push(image);
                continue;
            }
            self.admin.remove(&image).await?;
            tracing::info!(%image, why, "storage cleanup: deleted a finished disk");
            crate::db::audit(pool, None, None, "storage.gc.delete", &image, serde_json::json!({"why": why}))
                .await
                .ok();
            self.seen.lock().unwrap().remove(&image);
            report.deleted.push(image);
        }
        // Forget images that stopped being garbage (or are gone).
        self.seen.lock().unwrap().retain(|k, _| still_garbage.contains(k));
        Ok(report)
    }
}

/// Whether `image` belongs to something finished for good.
pub async fn verdict(pool: &sqlx::PgPool, image: &str) -> anyhow::Result<Verdict> {
    if let Some(rest) = image.strip_prefix("machine-") {
        let Ok(id) = Uuid::parse_str(rest) else { return Ok(Verdict::Keep) };
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM machines WHERE id = $1").bind(id).fetch_optional(pool).await?;
        return Ok(match state.as_deref() {
            None => Verdict::Garbage("its machine no longer exists"),
            Some("destroyed") => Verdict::Garbage("its machine was destroyed"),
            Some(_) => Verdict::Keep,
        });
    }
    let Ok(id) = Uuid::parse_str(image) else { return Ok(Verdict::Keep) };
    let row: Option<(String, bool)> =
        sqlx::query_as("SELECT state, archived_at IS NOT NULL FROM sessions WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        None => Verdict::Garbage("its session no longer exists"),
        Some((state, archived)) if archived || state == "reaped" => Verdict::Garbage("its session was archived"),
        Some(_) => Verdict::Keep,
    })
}

/// Run the sweep forever on whichever controld holds the cleanup lock.
pub fn spawn(state: crate::AppState) {
    let Some(shared) = state.shared_volumes.clone() else { return };
    let gc = StorageGc::new(shared.admin.clone(), shared.gc.clone());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(gc.policy.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut lock = crate::leases::SweepLock::with_key(STORAGE_GC_LOCK_KEY);
        loop {
            tick.tick().await;
            if !lock.hold(&state.pool).await {
                continue;
            }
            match gc.sweep_once(&state.pool).await {
                Ok(r) if r.deleted.is_empty() && r.open.is_empty() && r.would_delete.is_empty() => {}
                Ok(r) => tracing::info!(deleted = r.deleted.len(), waiting = r.waiting.len(), open = r.open.len(), would_delete = r.would_delete.len(), "storage cleanup sweep"),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "storage cleanup sweep failed"),
            }
        }
    });
}

/// Advisory-lock key for the storage cleanup leader ("pukustgc").
const STORAGE_GC_LOCK_KEY: i64 = 0x7075_6b75_7374_6763;
