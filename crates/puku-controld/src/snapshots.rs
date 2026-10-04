//! Machine snapshots on the control plane (docs/MACHINES-API.md,
//! "Snapshots"): ordering captures, brokering a worker's uploads, deciding
//! what a restore lays down, and the sweep that takes periodic snapshots and
//! deletes what retention no longer keeps.
//!
//! Bucket credentials never leave this process. A worker gets presigned part
//! URLs, uploads, and reports; this side starts and completes each multipart
//! upload, and is the only side that can delete anything.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use puku_cloud_proto::snapshot::{
    object_key, Consistency, Fingerprint, LayerOrder, PartDone, PartUrl, RestoreOrder, SnapshotLayer,
    SnapshotOrder, SnapshotProgress, SnapshotTrigger, FEATURE_SNAPSHOTS,
};
use puku_cloud_proto::worker_proto::Down;
use puku_cloud_proto::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::machines::{self as mdb, MachineRow};
use crate::db::snapshots::{self as sdb, Completed, NewSnapshot, ObjectRow, Ready, SnapshotRow};
use crate::workerlink::{send_to_worker, WorkerHandle};
use crate::AppState;

/// S3 numbers parts 1..=10000.
const MAX_PARTS: u32 = 10_000;
/// URLs handed out per request: enough to keep an upload busy, few enough
/// that a leaked batch is a small window.
const MAX_URLS_PER_REQUEST: u32 = 64;
/// A capture still open after this long is not coming back.
const STALE_CAPTURE_S: i64 = 6 * 3600;

/// Deployment-wide settings. `Config::snapshots` is `None` when the
/// deployment has no object storage, or no PUKU_SECRET_KEY to seal data keys
/// with: snapshots are then not offered at all.
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    pub part_bytes: u64,
    /// Ready snapshots kept per machine that does not say.
    pub keep: u32,
    /// Days a destroyed machine's snapshots outlive it.
    pub retain_destroyed_days: i64,
    pub sweep_s: u64,
}

/// A machine's own snapshot settings (`snapshots` on create). Everything is
/// off by default: a machine that asks for nothing is snapshotted only when
/// somebody asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPolicy {
    /// Snapshot every time the machine stops, an idle stop included.
    #[serde(default)]
    pub on_stop: bool,
    /// Seconds between snapshots while it runs; 0 is never.
    #[serde(default)]
    pub interval_s: u32,
    /// Ready snapshots to keep; the deployment's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<u32>,
    /// A last snapshot before the machine is destroyed.
    #[serde(default)]
    pub before_destroy: bool,
    /// tar patterns under the volume never captured: caches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl SnapshotPolicy {
    pub fn of(m: &MachineRow) -> Self {
        serde_json::from_value(m.snapshot_policy.clone()).unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.interval_s != 0 && self.interval_s < 300 {
            return Err("snapshots.interval_s must be 0 (off) or at least 300".into());
        }
        if self.keep.is_some_and(|k| !(1..=100).contains(&k)) {
            return Err("snapshots.keep must be between 1 and 100".into());
        }
        for e in &self.exclude {
            if e.is_empty() || e.starts_with('/') || e.split('/').any(|s| s == "..") || e.contains('\0') {
                return Err(format!("snapshots.exclude {e:?} must be a relative path without `..`"));
            }
        }
        Ok(())
    }
}

/// Whether this deployment takes snapshots at all.
pub fn enabled(state: &AppState) -> bool {
    state.cfg.snapshots.is_some() && state.blobs.is_some() && state.secrets.is_some()
}

fn can_snapshot(w: &WorkerHandle) -> bool {
    w.features.iter().any(|f| f == FEATURE_SNAPSHOTS)
}

/// The worker holding a machine's disks, if it is connected and can
/// snapshot: the one running it, else the one keeping its volume.
pub fn holder(state: &AppState, m: &MachineRow) -> Option<WorkerHandle> {
    let id = m.worker_id.or(m.volume_worker_id)?;
    state.workers.get(id).filter(can_snapshot)
}

/// What a machine has to capture: its volume, and the root disk it keeps.
fn layers_of(m: &MachineRow, snapshot_id: Uuid) -> Vec<LayerOrder> {
    let mut layers = Vec::new();
    let mut add = |layer| layers.push(LayerOrder { layer, key: object_key(m.id, snapshot_id, layer), sha256: None });
    if m.volume.is_some() {
        add(SnapshotLayer::Volume);
    }
    if m.persist_root && m.engine() == Engine::CloudHypervisor {
        add(SnapshotLayer::Root);
    }
    layers
}

/// Whether a machine has anything a snapshot would capture.
pub fn has_disks(m: &MachineRow) -> bool {
    !layers_of(m, Uuid::nil()).is_empty()
}

fn fingerprint_of(s: &SnapshotRow) -> Option<Fingerprint> {
    serde_json::from_value(s.manifest.as_ref()?.get("fingerprint")?.clone()).ok()
}

/// Write a snapshot's rows and return the order that captures it on
/// `worker_id`. Nothing is uploaded until the caller sends it.
pub async fn order(
    state: &AppState,
    m: &MachineRow,
    trigger: SnapshotTrigger,
    worker_id: Uuid,
    label: Option<&str>,
    pinned: bool,
) -> Result<SnapshotOrder> {
    let cfg = state.cfg.snapshots.as_ref().context("snapshots are not enabled on this deployment")?;
    let secrets = state.secrets.as_ref().context("snapshots need PUKU_SECRET_KEY")?;
    let id = Uuid::new_v4();
    let layers = layers_of(m, id);
    anyhow::ensure!(!layers.is_empty(), "the machine has no volume and no persistent root disk to snapshot");
    // One key per snapshot: a leaked key opens one snapshot, not a history.
    let key_hex = hex::encode(rand::random::<[u8; 32]>());
    let dek_enc = secrets.encrypt(&key_hex)?;
    let previous = if trigger.may_skip() {
        sdb::latest_ready(&state.pool, m.id).await?.as_ref().and_then(fingerprint_of)
    } else {
        None
    };
    sdb::insert(
        &state.pool,
        &NewSnapshot {
            id,
            machine: m,
            trigger,
            worker_id,
            dek_enc: &dek_enc,
            pinned,
            label,
            part_bytes: cfg.part_bytes as i64,
            layers: &layers,
        },
    )
    .await?;
    Ok(SnapshotOrder {
        snapshot_id: id,
        machine_id: m.id,
        trigger,
        key_hex,
        part_bytes: cfg.part_bytes,
        layers,
        excludes: SnapshotPolicy::of(m).exclude,
        previous,
    })
}

/// A capture to send with a stop or a destroy, when the deployment, the
/// machine and its worker all allow one. Never fails the stop: a snapshot
/// that cannot be ordered is logged and skipped.
pub async fn order_for(
    state: &AppState,
    m: &MachineRow,
    trigger: SnapshotTrigger,
    worker: &WorkerHandle,
) -> Option<SnapshotOrder> {
    if !enabled(state) || !can_snapshot(worker) || !has_disks(m) {
        return None;
    }
    match order(state, m, trigger, worker.worker_id, None, false).await {
        Ok(o) => Some(o),
        Err(e) => {
            tracing::warn!(machine = %m.id, error = format!("{e:#}"), "could not order a snapshot");
            None
        }
    }
}

/// Order a capture and send it now: a manual snapshot, or a periodic one.
pub async fn take(
    state: &AppState,
    m: &MachineRow,
    trigger: SnapshotTrigger,
    label: Option<&str>,
    pinned: bool,
) -> Result<SnapshotRow> {
    let w = holder(state, m).context("the worker holding this machine's disks is offline or cannot snapshot")?;
    let o = order(state, m, trigger, w.worker_id, label, pinned).await?;
    let id = o.snapshot_id;
    if !w.send(Down::SnapshotMachine { order: o }) {
        sdb::set_state(&state.pool, id, "failed", Some("the worker went away before it got the order")).await?;
    }
    sdb::get(&state.pool, id).await?.context("the snapshot row vanished")
}

// ------------------------------------------------------------ uploads

/// The snapshot a worker may upload into: ordered from it, and still open.
async fn open_for(state: &AppState, worker_id: Uuid, snapshot_id: Uuid) -> Result<SnapshotRow> {
    let s = sdb::get(&state.pool, snapshot_id).await?.context("no such snapshot")?;
    // The request comes from a worker: one may upload only into a snapshot
    // that was ordered from it, so a compromised worker cannot write into
    // another machine's history.
    anyhow::ensure!(s.worker_id == Some(worker_id), "snapshot {snapshot_id} was not ordered from this worker");
    anyhow::ensure!(s.is_open(), "snapshot {snapshot_id} is {}", s.state);
    Ok(s)
}

/// Answer a worker's `RequestSnapshotUrls`.
pub async fn on_request_urls(
    state: &AppState,
    worker_id: Uuid,
    snapshot_id: Uuid,
    layer: SnapshotLayer,
    first_part: u32,
    count: u32,
) -> Result<()> {
    let (urls, error) = match presign_parts(state, worker_id, snapshot_id, layer, first_part, count).await {
        Ok(urls) => (urls, None),
        Err(e) => {
            tracing::warn!(snapshot = %snapshot_id, layer = layer.as_str(), error = format!("{e:#}"), "refused snapshot part URLs");
            (Vec::new(), Some(format!("{e:#}")))
        }
    };
    send_to_worker(state, Some(worker_id), Down::SnapshotUrls { snapshot_id, layer, first_part, urls, error })
}

async fn presign_parts(
    state: &AppState,
    worker_id: Uuid,
    snapshot_id: Uuid,
    layer: SnapshotLayer,
    first_part: u32,
    count: u32,
) -> Result<Vec<PartUrl>> {
    let blobs = state.blobs.as_ref().context("no object storage is configured")?;
    open_for(state, worker_id, snapshot_id).await?;
    anyhow::ensure!(
        (1..=MAX_URLS_PER_REQUEST).contains(&count)
            && first_part >= 1
            && first_part.saturating_add(count - 1) <= MAX_PARTS,
        "parts {first_part}..+{count} are out of range"
    );
    let obj = sdb::object(&state.pool, snapshot_id, layer).await?.context("that layer is not part of this snapshot")?;
    let upload_id = match obj.upload_id {
        Some(u) => u,
        None => {
            let u = blobs.create_multipart(&obj.key).await?;
            sdb::set_upload_id(&state.pool, snapshot_id, layer, &u).await?;
            sdb::set_state(&state.pool, snapshot_id, "uploading", None).await?;
            u
        }
    };
    Ok((first_part..first_part + count)
        .map(|part| PartUrl { part, url: blobs.presign_upload_part(&obj.key, &upload_id, part as u16) })
        .collect())
}

/// What a worker says about a finished layer.
pub struct LayerDone {
    pub parts: Vec<PartDone>,
    pub plain_bytes: u64,
    pub stored_bytes: u64,
    pub sha256: String,
}

/// Complete a layer's multipart upload once its worker has sent every part.
pub async fn on_layer_done(
    state: &AppState,
    worker_id: Uuid,
    snapshot_id: Uuid,
    layer: SnapshotLayer,
    done: LayerDone,
) -> Result<()> {
    let blobs = state.blobs.as_ref().context("no object storage is configured")?;
    let obj = sdb::object(&state.pool, snapshot_id, layer).await?.context("that layer is not part of this snapshot")?;
    let upload_id = obj.upload_id.clone().context("no upload was started for that layer")?;
    if let Err(e) = open_for(state, worker_id, snapshot_id).await {
        // Purged or failed meanwhile: do not assemble an object that nothing
        // tracks and so nothing would ever delete.
        let _ = blobs.abort_multipart(&obj.key, &upload_id).await;
        return Err(e);
    }
    let mut parts = done.parts;
    parts.sort_by_key(|p| p.part);
    let completed = if parts.is_empty() || !parts.iter().enumerate().all(|(i, p)| p.part == i as u32 + 1) {
        Err(anyhow::anyhow!("the parts reported are not numbered 1..={}", parts.len()))
    } else {
        let etags: Vec<String> = parts.iter().map(|p| p.etag.clone()).collect();
        blobs.complete_multipart(&obj.key, &upload_id, &etags).await
    };
    match completed {
        Ok(()) => {
            let c = Completed {
                parts: parts.len() as i32,
                plain_bytes: done.plain_bytes as i64,
                stored_bytes: done.stored_bytes as i64,
                sha256: &done.sha256,
            };
            sdb::complete_object(&state.pool, snapshot_id, layer, &c).await
        }
        Err(e) => {
            tracing::warn!(snapshot = %snapshot_id, layer = layer.as_str(), error = format!("{e:#}"), "completing a snapshot layer failed");
            let _ = blobs.abort_multipart(&obj.key, &upload_id).await;
            sdb::fail_object(&state.pool, snapshot_id, &obj.layer).await
        }
    }
}

/// A worker's report on a capture (`Up::SnapshotState`).
pub struct Report {
    pub snapshot_id: Uuid,
    pub machine_id: Uuid,
    pub progress: SnapshotProgress,
    pub consistency: Option<Consistency>,
    pub error: Option<String>,
    pub fingerprint: Option<Fingerprint>,
    pub reused: Vec<SnapshotLayer>,
}

pub async fn on_state(state: &AppState, worker_id: Uuid, r: Report) -> Result<()> {
    let s = match open_for(state, worker_id, r.snapshot_id).await {
        Ok(s) if s.machine_id == r.machine_id => s,
        Ok(_) => anyhow::bail!("snapshot {} is not of machine {}", r.snapshot_id, r.machine_id),
        Err(e) => {
            tracing::debug!(snapshot = %r.snapshot_id, error = format!("{e:#}"), "late snapshot report ignored");
            return Ok(());
        }
    };
    let pool = &state.pool;
    match r.progress {
        SnapshotProgress::Uploading => {
            sdb::set_state(pool, s.id, "uploading", None).await?;
        }
        SnapshotProgress::Skipped => {
            for o in sdb::objects(pool, s.id).await? {
                sdb::drop_object(pool, s.id, &o.layer).await?;
            }
            sdb::set_state(pool, s.id, "skipped", None).await?;
            tracing::info!(snapshot = %s.id, machine = %s.machine_id, "snapshot skipped: nothing changed since the last one");
        }
        SnapshotProgress::Failed => {
            abort_uploads(state, &s).await?;
            let error = r.error.as_deref().unwrap_or("the capture failed");
            sdb::set_state(pool, s.id, "failed", Some(error)).await?;
            tracing::warn!(snapshot = %s.id, machine = %s.machine_id, error, "snapshot failed");
        }
        SnapshotProgress::Ready => finish(state, &s, r).await?,
    }
    Ok(())
}

/// Give up on a snapshot's uploads that never completed.
async fn abort_uploads(state: &AppState, s: &SnapshotRow) -> Result<()> {
    let Some(blobs) = &state.blobs else { return Ok(()) };
    for o in sdb::objects(&state.pool, s.id).await? {
        if let (Some(upload_id), false) = (&o.upload_id, o.is_stored()) {
            if let Err(e) = blobs.abort_multipart(&o.key, upload_id).await {
                tracing::debug!(key = %o.key, error = format!("{e:#}"), "aborting a snapshot upload");
            }
            sdb::fail_object(&state.pool, s.id, &o.layer).await?;
        }
    }
    Ok(())
}

async fn finish(state: &AppState, s: &SnapshotRow, r: Report) -> Result<()> {
    let pool = &state.pool;
    for layer in &r.reused {
        match sdb::reusable_layer(pool, s.machine_id, *layer, s.id).await? {
            Some(from) => sdb::reuse_object(pool, s.id, &from).await?,
            None => sdb::drop_object(pool, s.id, layer.as_str()).await?,
        }
    }
    abort_uploads(state, s).await?;
    // A layer neither uploaded nor reused is not part of this snapshot: a
    // live capture leaves the root disk out.
    let mut kept = Vec::new();
    for o in sdb::objects(pool, s.id).await? {
        if o.is_stored() {
            kept.push(o);
        } else {
            sdb::drop_object(pool, s.id, &o.layer).await?;
        }
    }
    if kept.is_empty() {
        sdb::set_state(pool, s.id, "failed", Some("nothing was captured")).await?;
        return Ok(());
    }
    let m = mdb::get(pool, s.machine_id).await?.context("the machine is gone")?;
    let consistency = r.consistency.unwrap_or(Consistency::Live);
    let manifest = manifest(state, s, &m, &kept, consistency, r.fingerprint.as_ref()).await;
    let size_bytes: i64 = kept.iter().filter_map(|o| o.plain_bytes).sum();
    let stored_bytes: i64 = kept.iter().filter_map(|o| o.stored_bytes).sum();
    sdb::mark_ready(
        pool,
        s.id,
        &Ready { consistency: consistency.as_str(), manifest: &manifest, size_bytes, stored_bytes },
    )
    .await?;
    tracing::info!(
        snapshot = %s.id, machine = %s.machine_id, consistency = consistency.as_str(),
        bytes = size_bytes, stored = stored_bytes, "snapshot ready"
    );
    Ok(())
}

/// Everything a restore needs to boot the machine again as it was. Secret
/// values are never in it, only their names.
async fn manifest(
    state: &AppState,
    s: &SnapshotRow,
    m: &MachineRow,
    kept: &[ObjectRow],
    consistency: Consistency,
    fingerprint: Option<&Fingerprint>,
) -> serde_json::Value {
    let secret_env_keys: Vec<String> = match (mdb::secret_env(&state.pool, m.id).await, &state.secrets) {
        (Ok(Some(enc)), Some(secrets)) => secrets
            .decrypt(&enc)
            .ok()
            .and_then(|plain| serde_json::from_str::<BTreeMap<String, String>>(&plain).ok())
            .map(|env| env.into_keys().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let layers: Vec<serde_json::Value> = kept
        .iter()
        .map(|o| {
            serde_json::json!({
                "layer": o.layer, "key": o.key, "plain_bytes": o.plain_bytes,
                "stored_bytes": o.stored_bytes, "sha256": o.sha256, "parts": o.parts,
                "part_bytes": o.part_bytes, "reused": o.state == "reused",
            })
        })
        .collect();
    serde_json::json!({
        "v": 1,
        "snapshot_id": s.id,
        "machine_id": s.machine_id,
        "created_at": s.created_at,
        "trigger": s.trigger,
        "consistency": consistency.as_str(),
        "engine": s.engine,
        "image": s.image,
        "cpus": s.cpus,
        "memory_mib": s.memory_mib,
        "expose": m.expose,
        "volume": m.volume,
        "env": m.env,
        "secret_env_keys": secret_env_keys,
        "entrypoint": m.entrypoint,
        "persist_root": m.persist_root,
        "fingerprint": fingerprint,
        "layers": layers,
    })
}

// ------------------------------------------------------------ restores

/// The restore order for a ready snapshot: where each layer is, and the key
/// that opens it.
pub async fn restore_order(state: &AppState, snapshot_id: Uuid) -> Result<RestoreOrder> {
    let secrets = state.secrets.as_ref().context("restoring needs PUKU_SECRET_KEY")?;
    let s = sdb::get(&state.pool, snapshot_id).await?.context("the snapshot is gone")?;
    anyhow::ensure!(s.state == "ready", "snapshot {snapshot_id} is {}, not ready", s.state);
    let key_hex = secrets.decrypt(&s.dek_enc)?;
    let layers = sdb::objects(&state.pool, snapshot_id)
        .await?
        .into_iter()
        .filter(ObjectRow::is_stored)
        .filter_map(|o| Some(LayerOrder { layer: o.layer()?, key: o.key, sha256: o.sha256 }))
        .collect();
    Ok(RestoreOrder { snapshot_id, key_hex, layers })
}

/// Answer a worker's `RequestSnapshotGet`.
pub async fn on_request_get(state: &AppState, worker_id: Uuid, snapshot_id: Uuid, layer: SnapshotLayer) -> Result<()> {
    let url = match presign_get(state, worker_id, snapshot_id, layer).await {
        Ok(url) => Some(url),
        Err(e) => {
            tracing::warn!(snapshot = %snapshot_id, error = format!("{e:#}"), "refused a snapshot download");
            None
        }
    };
    send_to_worker(state, Some(worker_id), Down::SnapshotGetUrl { snapshot_id, layer, url })
}

async fn presign_get(state: &AppState, worker_id: Uuid, snapshot_id: Uuid, layer: SnapshotLayer) -> Result<String> {
    let blobs = state.blobs.as_ref().context("no object storage is configured")?;
    let s = sdb::get(&state.pool, snapshot_id).await?.context("no such snapshot")?;
    let m = mdb::get(&state.pool, s.machine_id).await?.context("the machine is gone")?;
    // Only the worker restoring this very snapshot onto its machine reads it.
    anyhow::ensure!(
        m.restore_snapshot_id == Some(snapshot_id) && m.worker_id == Some(worker_id),
        "this worker is not restoring snapshot {snapshot_id}"
    );
    let o = sdb::object(&state.pool, snapshot_id, layer)
        .await?
        .filter(ObjectRow::is_stored)
        .context("that layer is not in this snapshot")?;
    Ok(blobs.presign_get(&o.key))
}

/// Once a restored boot runs, tell the worker whose copy it replaced to drop
/// it. One that is offline hears it when it reconnects
/// (`db::machines::reapable`).
pub async fn reap_stale_copy(state: &AppState, m: &MachineRow, running_on: Uuid) -> Result<()> {
    let Some(stale) = m.stale_worker_id.filter(|w| *w != running_on) else { return Ok(()) };
    let Some(w) = state.workers.get(stale).filter(can_snapshot) else { return Ok(()) };
    if w.send(Down::ReapMachine { machine_id: m.id, below_generation: m.generation as u64 }) {
        mdb::clear_stale(&state.pool, m.id).await?;
        tracing::info!(machine = %m.id, worker = %w.name, "told the previous worker to drop its copy");
    }
    Ok(())
}

// -------------------------------------------------------------- sweep

/// Periodic captures, retention and deletion. Runs forever.
pub fn spawn_sweep(state: AppState) {
    let Some(cfg) = state.cfg.snapshots.clone() else { return };
    let every = std::time::Duration::from_secs(cfg.sweep_s.max(5));
    puku_observability::supervise("snapshot_sweep", move || {
        let state = state.clone();
        let cfg = cfg.clone();
        async move {
            loop {
                tokio::time::sleep(every).await;
                if let Err(e) = sweep(&state, &cfg).await {
                    tracing::warn!(error = format!("{e:#}"), "snapshot sweep failed");
                }
            }
        }
    });
}

/// One pass: periodic captures, captures that never finished, retention,
/// then deleting what nothing keeps.
pub async fn sweep(state: &AppState, cfg: &SnapshotConfig) -> Result<()> {
    if !enabled(state) {
        return Ok(());
    }
    for m in mdb::due_for_snapshot(&state.pool).await? {
        if let Err(e) = take(state, &m, SnapshotTrigger::Periodic, None, false).await {
            tracing::debug!(machine = %m.id, error = format!("{e:#}"), "periodic snapshot not taken");
        }
    }
    for s in sdb::stale_open(&state.pool, STALE_CAPTURE_S).await? {
        abort_uploads(state, &s).await?;
        sdb::set_state(&state.pool, s.id, "failed", Some("the capture never finished")).await?;
    }
    for id in sdb::over_retention(&state.pool, cfg.keep as i32).await? {
        sdb::mark_deleting(&state.pool, id).await?;
    }
    for id in sdb::of_destroyed_machines(&state.pool, cfg.retain_destroyed_days).await? {
        sdb::mark_deleting(&state.pool, id).await?;
    }
    sdb::expire_leftovers(&state.pool).await?;
    collect(state).await
}

/// Delete the objects of snapshots on their way out. An object another
/// snapshot still names (a reused layer) is left for that one to delete.
async fn collect(state: &AppState) -> Result<()> {
    let Some(blobs) = &state.blobs else { return Ok(()) };
    for s in sdb::deleting(&state.pool, 50).await? {
        for o in sdb::objects(&state.pool, s.id).await? {
            if o.state == "deleted" {
                continue;
            }
            if let Some(upload_id) = o.upload_id.as_deref().filter(|_| !o.is_stored()) {
                let _ = blobs.abort_multipart(&o.key, upload_id).await;
            }
            if o.is_stored() && !sdb::key_in_use(&state.pool, &o.key, s.id).await? {
                blobs.delete_object(&o.key).await?;
            }
            sdb::object_deleted(&state.pool, s.id, &o.layer).await?;
        }
        sdb::finish_delete(&state.pool, s.id).await?;
        tracing::info!(snapshot = %s.id, machine = %s.machine_id, "snapshot deleted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_are_validated() {
        assert!(SnapshotPolicy::default().validate().is_ok());
        let every = |s| SnapshotPolicy { interval_s: s, ..Default::default() };
        assert!(every(60).validate().is_err(), "more often than every 5 minutes is refused");
        assert!(every(900).validate().is_ok());
        let keep = |k| SnapshotPolicy { keep: Some(k), ..Default::default() };
        assert!(keep(0).validate().is_err());
        assert!(keep(5).validate().is_ok());
        let exclude = |e: &str| SnapshotPolicy { exclude: vec![e.into()], ..Default::default() };
        assert!(exclude(".cache").validate().is_ok());
        assert!(exclude("/etc").validate().is_err());
        assert!(exclude("a/../b").validate().is_err());
    }

    /// A machine created before policies existed reads as all off.
    #[test]
    fn an_empty_policy_is_all_off() {
        let p: SnapshotPolicy = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p, SnapshotPolicy::default());
        assert!(!p.on_stop && !p.before_destroy && p.interval_s == 0);
    }
}
