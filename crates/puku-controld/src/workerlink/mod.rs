//! Worker link: the `/v1/worker` WebSocket endpoint workers dial into, and
//! the in-memory registry used to push Down frames to them.

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::event::EventKind;
use puku_cloud_proto::session::SessionState;
use puku_cloud_proto::worker_proto::{Down, HostReport, StopMode, Up, FEATURE_MACHINES};
use puku_cloud_proto::Engine;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{db, AppState};

#[derive(Clone)]
pub struct WorkerHandle {
    pub worker_id: Uuid,
    pub name: String,
    pub capacity_slots: u32,
    pub used_slots: u32,
    pub draining: bool,
    /// Sandboxes this host last reported. `None` until the first heartbeat
    /// from a worker new enough to send them — distinct from `Some(vec![])`,
    /// which genuinely means "this host has none".
    pub sandboxes: Option<Vec<String>>,
    /// Engines this worker can boot. A worker that advertised none predates
    /// engines and is recorded as libkrun-only, never as "anything".
    pub engines: Vec<Engine>,
    /// Optional frame families this worker understands (see `Up::Register`).
    pub features: Vec<String>,
    /// The host's limits and staged images, as last reported. Empty for an
    /// older worker, and an empty report never refuses anything.
    pub host: HostReport,
    /// Hash of the token this worker registered its control link with. Its data
    /// sockets present the same one, which is checked without a database round trip.
    pub token_hash: Vec<u8>,
    tx: mpsc::UnboundedSender<Down>,
}

impl WorkerHandle {
    pub fn send(&self, frame: Down) -> bool {
        self.tx.send(frame).is_ok()
    }

    /// Whether this worker can take the placement now, and if not, the most
    /// fundamental reason why.
    fn check(&self, p: &Placement) -> Result<(), Refusal> {
        if !self.engines.contains(&p.engine) {
            return Err(Refusal::Engine);
        }
        if !p.features.iter().all(|f| self.features.iter().any(|have| have == f)) {
            return Err(Refusal::Feature);
        }
        match p.pinned {
            // The volumes are on this host and nowhere else, so a pinned
            // placement goes here even while it drains: draining stops *new*
            // work, and refusing a resume would strand the session until an
            // undrain nobody knows to do.
            Some(id) if self.worker_id != id => return Err(Refusal::NotPinned),
            Some(_) => {}
            None if self.draining => return Err(Refusal::Draining),
            None => {}
        }
        if self.too_large(p) {
            return Err(Refusal::TooLarge);
        }
        if let (Some(key), Some(staged)) = (&p.image_key, &self.host.images) {
            if !staged.iter().any(|i| i.engine == p.engine && &i.key == key) {
                return Err(Refusal::Image);
            }
        }
        if p.disk_mib > 0 && self.host.disk_free_mib.is_some_and(|free| free < p.disk_mib) {
            return Err(Refusal::Disk);
        }
        if self.used_slots + p.slots() > self.capacity_slots {
            return Err(Refusal::Full);
        }
        Ok(())
    }

    /// Could never fit here, however empty the host. A limit the worker did
    /// not report is never grounds for this: an older worker is only ever
    /// "full", which is all it could be before hosts reported limits.
    fn too_large(&self, p: &Placement) -> bool {
        self.host.max_slots.is_some_and(|max| p.slots() > max) || self.host.cores.is_some_and(|c| p.cpus > c)
    }

    fn free_slots(&self) -> u32 {
        self.capacity_slots.saturating_sub(self.used_slots)
    }
}

/// Why one worker cannot take a placement. Declared closest-to-fitting
/// first: across a fleet, `diagnose` reports the refusal of whichever worker
/// came nearest, because that is the one worth acting on -- "full right now"
/// beats "the other box is too small".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Refusal {
    Full,
    Disk,
    Image,
    TooLarge,
    Draining,
    Feature,
    Engine,
    /// Pinned to another worker. A filter, never a reason.
    NotPinned,
}

/// What a VM needs from the worker it lands on.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    pub engine: Engine,
    /// Only this worker will do, because it holds the volumes.
    pub pinned: Option<Uuid>,
    /// Frame families the worker must have advertised (machines, snapshots).
    pub features: &'static [&'static str],
    /// Capacity slots the VM occupies; 0 means the one a session takes.
    pub slots: u32,
    /// vCPUs, checked against the host's cores; 0 when not given.
    pub cpus: u32,
    /// `machine::image_key` of an image the worker must have staged, for an
    /// engine that boots a prepared disk. `None` for engines that pull.
    pub image_key: Option<String>,
    /// Free disk the worker must have, in MiB; 0 for no requirement.
    pub disk_mib: u64,
}

impl Placement {
    pub fn slots(&self) -> u32 {
        self.slots.max(1)
    }
}

/// Why nothing in the fleet can take a placement right now. Each variant is
/// one machine-readable `reason` on the API (docs/MACHINES-API.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unplaceable {
    NoWorkers,
    EngineUnavailable { engine: Engine },
    MissingFeature { feature: &'static str },
    AllDraining,
    TooLarge { requested_slots: u32, cpus: u32, max_slots: Option<u32>, max_cores: Option<u32> },
    ImageNotStaged { image_key: String },
    InsufficientDisk { needed_mib: u64, best_free_mib: Option<u64> },
    CapacityFull { requested_slots: u32, best_free_slots: u32, workers: usize },
    VolumeHostOffline { worker_id: Uuid },
    VolumeHostFull { worker_id: Uuid, requested_slots: u32, free_slots: u32 },
}

impl Unplaceable {
    pub fn reason(&self) -> &'static str {
        match self {
            Unplaceable::NoWorkers => "no_workers",
            Unplaceable::EngineUnavailable { .. } => "engine_unavailable",
            Unplaceable::MissingFeature { feature } if *feature == FEATURE_MACHINES => "machines_unsupported",
            Unplaceable::MissingFeature { .. } => "feature_unsupported",
            Unplaceable::AllDraining => "all_draining",
            Unplaceable::TooLarge { .. } => "too_large",
            Unplaceable::ImageNotStaged { .. } => "image_not_staged",
            Unplaceable::InsufficientDisk { .. } => "insufficient_disk",
            Unplaceable::CapacityFull { .. } => "capacity_full",
            Unplaceable::VolumeHostOffline { .. } => "volume_host_offline",
            Unplaceable::VolumeHostFull { .. } => "volume_host_full",
        }
    }
}

/// The fleet-wide reason for the refusal the nearest workers gave.
fn explain(r: Refusal, p: &Placement, at: &[&WorkerHandle]) -> Unplaceable {
    match r {
        Refusal::Full => Unplaceable::CapacityFull {
            requested_slots: p.slots(),
            best_free_slots: at.iter().map(|w| w.free_slots()).max().unwrap_or(0),
            workers: at.len(),
        },
        Refusal::Disk => Unplaceable::InsufficientDisk {
            needed_mib: p.disk_mib,
            best_free_mib: at.iter().filter_map(|w| w.host.disk_free_mib).max(),
        },
        Refusal::Image => Unplaceable::ImageNotStaged { image_key: p.image_key.clone().unwrap_or_default() },
        Refusal::TooLarge => Unplaceable::TooLarge {
            requested_slots: p.slots(),
            cpus: p.cpus,
            max_slots: at.iter().filter_map(|w| w.host.max_slots).max(),
            max_cores: at.iter().filter_map(|w| w.host.cores).max(),
        },
        Refusal::Draining => Unplaceable::AllDraining,
        // The first feature none of the nearest workers has.
        Refusal::Feature => Unplaceable::MissingFeature {
            feature: p
                .features
                .iter()
                .copied()
                .find(|f| at.iter().all(|w| !w.features.iter().any(|have| have == f)))
                .unwrap_or(FEATURE_MACHINES),
        },
        Refusal::Engine | Refusal::NotPinned => Unplaceable::EngineUnavailable { engine: p.engine },
    }
}

/// A worker's advertised engines as the registry records them: absent means
/// libkrun, and names this build cannot run are dropped rather than trusted.
pub fn effective_engines(advertised: &[Engine]) -> Vec<Engine> {
    if advertised.is_empty() {
        return vec![Engine::Libkrun];
    }
    let mut out: Vec<Engine> = Vec::new();
    for e in advertised.iter().copied().filter(|e| *e != Engine::Unsupported) {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

#[derive(Clone)]
pub struct WorkerRegistry {
    inner: Arc<Mutex<HashMap<Uuid, WorkerHandle>>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        WorkerRegistry { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn insert(&self, handle: WorkerHandle) {
        self.inner.lock().unwrap().insert(handle.worker_id, handle);
    }

    pub fn remove(&self, worker_id: Uuid) {
        self.inner.lock().unwrap().remove(&worker_id);
    }

    /// Workers with a live link to *this* controld instance. Deliberately
    /// not the `workers` table count: a row can say online while the socket
    /// is gone, and that gap is what an operator needs to see.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, worker_id: Uuid) -> Option<WorkerHandle> {
        self.inner.lock().unwrap().get(&worker_id).cloned()
    }

    /// Least-loaded online worker with free capacity; draining workers
    /// accept no new sessions.
    /// Pick a worker with a free slot **and claim it**.
    ///
    /// The claim is the whole point. `used_slots` is otherwise only refreshed
    /// by the heartbeat, so a dispatch loop with several queued sessions
    /// would hand them all to the same worker before its first heartbeat
    /// landed -- measured: six microVMs on a worker configured for two, which
    /// on a real box is ~12 GB against a 4 GB budget.
    ///
    /// Claiming under the same lock as the choice makes over-subscription
    /// impossible rather than unlikely. The count can only ever drift *high*
    /// (a dispatch that fails after claiming), and the next heartbeat
    /// reconciles it -- erring toward refusing work rather than accepting
    /// work the box cannot run.
    ///
    /// Only workers that advertised the VM's engine qualify. That filter is
    /// what makes an engine field safe to add at all: a worker that has never
    /// heard of engines ignores the field and would boot libkrun for a spec
    /// that asked for something else.
    pub fn pick(&self, placement: &Placement) -> Option<WorkerHandle> {
        let mut g = self.inner.lock().unwrap();
        let id = g
            .values()
            .filter(|w| w.check(placement).is_ok())
            .min_by_key(|w| w.used_slots)
            .map(|w| w.worker_id)?;
        let w = g.get_mut(&id)?;
        w.used_slots += placement.slots();
        Some(w.clone())
    }

    /// Whether any connected worker can boot this engine at all, full or not.
    pub fn any_runs(&self, engine: Engine) -> bool {
        self.inner.lock().unwrap().values().any(|w| w.engines.contains(&engine))
    }

    /// Why [`pick`](Self::pick) would find nobody for this placement, without
    /// claiming anything; `Ok` when it would find someone now.
    ///
    /// The API asks this before it creates or starts a machine, so a request
    /// the fleet cannot serve is refused at once, with a reason, instead of
    /// waiting out its whole `wait_s` in `scheduled`.
    pub fn diagnose(&self, p: &Placement) -> Result<(), Unplaceable> {
        let g = self.inner.lock().unwrap();
        // The volumes decide. A pinned placement is about one worker, and
        // "no workers" would hide that the one it needs is the one missing.
        if let Some(pin) = p.pinned {
            let Some(w) = g.get(&pin) else {
                return Err(Unplaceable::VolumeHostOffline { worker_id: pin });
            };
            return match w.check(p) {
                Ok(()) => Ok(()),
                Err(Refusal::Full) => Err(Unplaceable::VolumeHostFull {
                    worker_id: pin,
                    requested_slots: p.slots(),
                    free_slots: w.free_slots(),
                }),
                Err(r) => Err(explain(r, p, &[w])),
            };
        }
        let mut nearest: Option<Refusal> = None;
        for w in g.values() {
            match w.check(p) {
                Ok(()) => return Ok(()),
                Err(r) => nearest = Some(nearest.map_or(r, |n| n.min(r))),
            }
        }
        let Some(r) = nearest else { return Err(Unplaceable::NoWorkers) };
        let at: Vec<&WorkerHandle> = g.values().filter(|w| w.check(p) == Err(r)).collect();
        Err(explain(r, p, &at))
    }

    /// Replace a worker's host report with a fresher one.
    pub fn note_host(&self, worker_id: Uuid, host: HostReport) {
        if let Some(w) = self.inner.lock().unwrap().get_mut(&worker_id) {
            w.host = host;
        }
    }

    pub fn note_slots(&self, worker_id: Uuid, used: u32) {
        if let Some(w) = self.inner.lock().unwrap().get_mut(&worker_id) {
            w.used_slots = used;
        }
    }

    /// Update a worker's advertised capacity mid-connection.
    pub fn note_capacity(&self, worker_id: Uuid, capacity: u32) {
        if let Some(w) = self.inner.lock().unwrap().get_mut(&worker_id) {
            w.capacity_slots = capacity;
        }
    }

    pub fn note_sandboxes(&self, worker_id: Uuid, sandboxes: Vec<String>) {
        if let Some(w) = self.inner.lock().unwrap().get_mut(&worker_id) {
            w.sandboxes = Some(sandboxes);
        }
    }

    pub fn all(&self) -> Vec<WorkerHandle> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    /// Returns false when the worker is not connected.
    pub fn set_draining(&self, worker_id: Uuid, draining: bool) -> bool {
        match self.inner.lock().unwrap().get_mut(&worker_id) {
            Some(w) => {
                w.draining = draining;
                true
            }
            None => false,
        }
    }
}

/// Drive one worker connection: authenticate the Register frame, ack with
/// resume cursors, then pump frames both ways until disconnect.
pub async fn handle_worker_socket(state: AppState, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // First frame must be Register with a valid token.
    let reg = match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Up>(&text) {
            Ok(Up::Register {
                worker_name,
                auth_token,
                capacity_slots,
                msb_version,
                running_sessions,
                on_disk_sessions,
                engines,
                features,
                running_machines,
                on_disk_machines,
                host,
            }) => {
                let auth = crate::workertoken::authenticate(
                    &state.pool,
                    &worker_name,
                    &auth_token,
                    &state.cfg.worker_token,
                    state.cfg.allow_shared_worker_token,
                )
                .await;
                let token_id = match auth {
                    Ok(Ok(crate::workertoken::WorkerAuth::Token { token_id })) => Some(token_id),
                    Ok(Ok(crate::workertoken::WorkerAuth::Shared)) => {
                        tracing::warn!(
                            worker = %worker_name,
                            "worker registered with the shared token; mint a per-worker one \
                             with `puku-controld gen-worker-token` and unset \
                             PUKU_ALLOW_SHARED_WORKER_TOKEN"
                        );
                        None
                    }
                    Ok(Err(reason)) => {
                        tracing::warn!(worker = %worker_name, ?reason, "worker auth failed");
                        return;
                    }
                    Err(e) => {
                        tracing::error!(worker = %worker_name, error = %e, "worker auth lookup failed");
                        return;
                    }
                };
                let token_hash = crate::auth::hash_key(&auth_token);
                let caps = (effective_engines(&engines), features, running_machines, on_disk_machines, host);
                (worker_name, capacity_slots, msb_version, running_sessions, token_id, token_hash, on_disk_sessions, caps)
            }
            _ => {
                tracing::warn!("first worker frame was not Register");
                return;
            }
        },
        _ => return,
    };
    let (
        worker_name,
        capacity_slots,
        msb_version,
        running_sessions,
        token_id,
        token_hash,
        on_disk_sessions,
        (engines, features, running_machines, on_disk_machines, host),
    ) = reg;
    let engine_names: Vec<String> = engines.iter().map(|e| e.as_str().to_string()).collect();

    let worker_id = match db::upsert_worker(
        &state.pool,
        &worker_name,
        capacity_slots as i32,
        &msb_version,
        token_id,
        &engine_names,
        &features,
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "worker upsert failed");
            return;
        }
    };
    let cursors = db::resume_cursors(&state.pool, &running_sessions)
        .await
        .unwrap_or_default();

    let wants_lease = features.iter().any(|f| f == puku_cloud_proto::worker_proto::FEATURE_LEASE);
    let (tx, mut rx) = mpsc::unbounded_channel::<Down>();
    let handle = WorkerHandle {
        worker_id,
        name: worker_name.clone(),
        capacity_slots,
        used_slots: 0,
        draining: false,
        sandboxes: None,
        engines,
        features,
        host: host.unwrap_or_default(),
        token_hash,
        tx,
    };
    state.workers.insert(handle);
    tracing::info!(worker = %worker_name, %worker_id, engines = ?engine_names, "worker online");

    // The host is talking to us, so whatever its lease row says, it is
    // alive: take the lease over under a new generation. A worker that
    // sends no renewals gets no lease row at all, so the sweeper never
    // declares it dead on the strength of a row nobody renews.
    let leases = crate::leases::service_for(&state);
    let mut lease = if wants_lease {
        match leases.takeover(worker_id, &state.cfg.instance_id.to_string()).await {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::error!(worker = %worker_name, error = %e, "lease takeover failed");
                None
            }
        }
    } else {
        let store = crate::leases::PgLeaseStore { pool: state.pool.clone() };
        let _ = puku_leases::LeaseStore::delete(&store, worker_id).await;
        None
    };

    // Of what the worker still has on disk, tell it which are already
    // archived. Reaping at archival time only reaches a worker that happens
    // to be connected; this is what makes a restarted or renamed worker
    // reclaim the volumes it has been sitting on.
    let reapable: Vec<Uuid> = if on_disk_sessions.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM sessions WHERE id = ANY($1) AND archived_at IS NOT NULL",
        )
        .bind(&on_disk_sessions)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
    };
    if !reapable.is_empty() {
        tracing::info!(worker = %worker_name, count = reapable.len(), "telling the worker to reclaim archived volumes");
    }
    let reapable_machines = db::machines::reapable(&state.pool, worker_id, &on_disk_machines)
        .await
        .unwrap_or_default();
    let ack = Down::RegisterAck { worker_id, resume_cursors: cursors, reapable, reapable_machines };
    if sink
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await
        .is_err()
    {
        state.workers.remove(worker_id);
        return;
    }

    // Outbound pump: registry mpsc -> websocket.
    let send_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let text = serde_json::to_string(&frame).unwrap();
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Sessions the worker still runs but the platform has already ended
    // (canceled/failed while the worker was away) get killed on reconnect.
    for sid in &running_sessions {
        let ended = match db::get_session(&state.pool, *sid).await {
            Ok(Some(s)) => s.session_state().is_terminal() || s.session_state() == puku_cloud_proto::session::SessionState::Stopped,
            Ok(None) => true,
            Err(_) => false,
        };
        if ended {
            tracing::info!(session = %sid, "reaping orphaned sandbox on worker reconnect");
            let _ = state.workers.get(worker_id).map(|w| {
                w.send(Down::StopSession { session_id: *sid, mode: puku_cloud_proto::worker_proto::StopMode::Kill })
            });
        }
    }

    if let Err(e) = reconcile_machines(&state, worker_id, &running_machines).await {
        tracing::warn!(worker = %worker_name, error = format!("{e:#}"), "reconciling machines failed");
    }

    // Newly-online worker may unblock queued sessions.
    if let Err(e) = crate::api::dispatch_pending(&state).await {
        tracing::warn!(error = %e, "dispatch after register failed");
    }

    // Inbound pump.
    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let frame = match serde_json::from_str::<Up>(&text) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "bad worker frame");
                continue;
            }
        };
        if matches!(frame, Up::LeaseRenew) {
            let Some(held) = lease.as_ref() else { continue };
            match leases.renew(held).await {
                Ok(next) => lease = Some(next),
                // Declared dead while the link was up (renewals stalled past
                // the grace period), or the host re-registered on another
                // link. Either way this link is stale: drop it, and the
                // worker reconnects under a new generation.
                Err(e @ (puku_leases::LeaseError::Dead(_) | puku_leases::LeaseError::NotHeld)) => {
                    tracing::warn!(worker = %worker_name, error = %e, "lease lost on a live link; dropping it");
                    lease = None;
                    break;
                }
                Err(e) => tracing::warn!(worker = %worker_name, error = %e, "lease renew failed"),
            }
            continue;
        }
        if let Err(e) = handle_up_frame(&state, worker_id, frame).await {
            tracing::error!(error = %e, "handling worker frame failed");
        }
    }

    tracing::info!(worker = %worker_name, "worker disconnected");
    if let Some(held) = lease.as_ref() {
        // Suspect it on the next sweep, not after the TTL.
        let _ = leases.expire_now(held).await;
    }
    state.workers.remove(worker_id);
    state.data.forget(worker_id);
    let _ = db::worker_offline(&state.pool, worker_id).await;
    // Anything handed to this worker that it never booted goes back in the
    // queue; the assignment frame died with the link.
    if let Ok(n) = db::machines::release_unbooted(&state.pool, worker_id, &[]).await {
        if n > 0 {
            tracing::info!(worker = %worker_name, count = n, "requeued undelivered machine assignments");
        }
    }
    send_task.abort();
}

/// Bring a reconnecting worker and the machines table back into agreement.
///
/// Three disagreements are possible, and each has one right answer:
/// the worker runs a VM the table says should not be running there (stop or
/// destroy it); the table says a VM runs there and the worker no longer has
/// it (it died with the worker: mark it stopped, volume intact); and an
/// assignment that never arrived (requeue it).
async fn reconcile_machines(state: &AppState, worker_id: Uuid, running: &[Uuid]) -> anyhow::Result<()> {
    let Some(handle) = state.workers.get(worker_id) else { return Ok(()) };
    for id in running {
        match db::machines::get(&state.pool, *id).await? {
            Some(m)
                if m.worker_id == Some(worker_id)
                    && matches!(m.state.as_str(), "restoring" | "booting" | "running" | "stopping") =>
            {
                if m.state == "stopping" {
                    handle.send(Down::StopMachine { machine_id: *id, generation: m.generation as u64, snapshot: None });
                }
            }
            Some(m) if m.state != "destroyed" => {
                handle.send(Down::StopMachine { machine_id: *id, generation: m.generation as u64, snapshot: None });
            }
            _ => {
                handle.send(Down::DestroyMachine { machine_id: *id, final_snapshot: None });
            }
        }
    }
    for lost in db::machines::stop_lost(&state.pool, worker_id, running).await? {
        tracing::warn!(machine = %lost.id, "machine VM lost while its worker was away; marked stopped");
        db::machines::close_runs(&state.pool, lost.id).await?;
    }
    db::machines::release_unbooted(&state.pool, worker_id, running).await?;
    Ok(())
}

async fn handle_up_frame(state: &AppState, worker_id: Uuid, frame: Up) -> anyhow::Result<()> {
    match frame {
        Up::Register { .. } => {} // only valid as first frame
        Up::LeaseRenew => {} // handled on the link, which owns the lease
        Up::Heartbeat { used_slots, capacity_slots, sandboxes, host } => {
            state.workers.note_sandboxes(worker_id, sandboxes);
            if let Some(host) = host {
                state.workers.note_host(worker_id, host);
            }
            state.workers.note_slots(worker_id, used_slots);
            // A worker that sizes itself from its own memory re-reports
            // capacity as the box fills and frees. Absent on older workers,
            // which keep whatever they registered with.
            if let Some(cap) = capacity_slots {
                state.workers.note_capacity(worker_id, cap);
            }
            db::worker_heartbeat(&state.pool, worker_id, used_slots as i32).await?;
        }
        Up::SessionEvents { session_id, mut events } => {
            // Workers name blobs by key; only controld knows the bucket, so
            // it stamps the fully-qualified reference. Stored self-describing
            // (`s3://bucket/key`) so a reference stays resolvable if the
            // endpoint or bucket changes later.
            if let Some(blobs) = &state.blobs {
                for ev in &mut events {
                    if let Some(key) = ev.blob_ref.as_ref().filter(|r| !r.contains("://")) {
                        ev.blob_ref = Some(blobs.blob_ref(key));
                    }
                }
            }
            let persisted = db::insert_guest_events(&state.pool, session_id, &events).await?;
            state.publish_events(&persisted).await;
        }
        Up::SessionState { session_id, state: next, error, puku_session_id } => {
            // A worker reporting progress has laid the session's volumes
            // out on its own disk, so that is where a resume has to go.
            // Failed/canceled are excluded: a worker can refuse a session
            // (an engine it does not run) before creating anything, and
            // pinning to it then would strand every later resume.
            if !matches!(next, SessionState::Failed | SessionState::Canceled) {
                db::note_volume_worker(&state.pool, session_id, worker_id).await?;
            }
            if let Some(psid) = &puku_session_id {
                db::set_puku_session_id(&state.pool, session_id, psid).await?;
            }
            match db::transition(&state.pool, session_id, next, error.as_deref()).await {
                Ok((row, ev)) => {
                    if let Some(ev) = ev {
                        state.publish_events(&[ev]).await;
                    }
                    if row.session_state().is_terminal() {
                        if let Err(e) = db::record_usage(&state.pool, session_id).await {
                            tracing::warn!(%session_id, error = %e, "usage record failed");
                        }
                        crate::memory::spawn_ingest(state, row.clone());
                        crate::notify::spawn(state, row, crate::notify::Trigger::Terminal);
                    }
                }
                Err(e) => tracing::warn!(%session_id, error = %e, "worker state transition rejected"),
            }
        }
        Up::PendingQuestion { session_id, question } => {
            sqlx::query("UPDATE sessions SET pending_question = $2 WHERE id = $1")
                .bind(session_id)
                .bind(&question)
                .execute(&state.pool)
                .await?;
            match db::transition(&state.pool, session_id, SessionState::WaitingInput, None).await {
                Ok((_, Some(ev))) => state.publish_events(&[ev]).await,
                Ok((_, None)) => {}
                Err(e) => tracing::debug!(%session_id, error = %e, "waiting_input transition skipped"),
            }
            let ev = db::insert_platform_event(
                &state.pool,
                session_id,
                EventKind::Session,
                serde_json::json!({"type": "session.question", "question": question}),
            )
            .await?;
            state.publish_events(&[ev]).await;
            // The whole reason notifications exist: nobody is watching an
            // unattended session, and this one will now wait forever.
            crate::notify::on_transition(state, session_id, "waiting_input").await;
        }
        Up::SessionUsage {
            session_id,
            cost_usd,
            tokens_in,
            tokens_out,
            cache_read_tokens,
            cache_write_tokens,
        } => {
            db::update_usage(
                &state.pool,
                session_id,
                cost_usd,
                tokens_in,
                tokens_out,
                cache_read_tokens,
                cache_write_tokens,
            )
            .await?;
            let ev = db::insert_platform_event(
                &state.pool,
                session_id,
                EventKind::Session,
                serde_json::json!({
                    "type": "session.usage",
                    "cost_usd": cost_usd,
                    "tokens_in": tokens_in,
                    "tokens_out": tokens_out,
                    "cache_read_tokens": cache_read_tokens,
                    "cache_write_tokens": cache_write_tokens,
                }),
            )
            .await?;
            state.publish_events(&[ev]).await;
            enforce_budget(state, session_id).await;
        }
        Up::RequestGitToken { session_id } => {
            // Minted fresh: the clone token was issued when the session
            // started and an installation token only lives an hour.
            //
            // The fallback goes through `operator_credential` for the same
            // reason `build_spec` does. This branch used to read
            // `cfg.git_token` directly, which meant a deployment with
            // PUKU_ALLOW_OPERATOR_CREDENTIALS off still handed the operator's
            // PAT to any session that asked for a push token -- withheld at
            // dispatch, granted an hour later. The spec path was tested; this
            // one was not, which is exactly why it drifted.
            let token = match db::get_session(&state.pool, session_id).await {
                Ok(Some(s)) => match (&*state.github, &s.repo) {
                    (Some(app), Some(repo)) => app.installation_token(repo).await.ok(),
                    _ => state.cfg.operator_credential(&state.cfg.git_token),
                },
                _ => None,
            };
            let _ = send_to_worker(state, Some(worker_id), Down::GitToken { session_id, token });
        }
        Up::BranchPushed { session_id, branch } => {
            if let Err(e) = open_pr_for(state, session_id, &branch).await {
                tracing::warn!(%session_id, %branch, error = format!("{e:#}"), "opening the pull request failed");
                if let Ok(ev) = db::insert_platform_event(
                    &state.pool,
                    session_id,
                    EventKind::Session,
                    serde_json::json!({
                        "type": "session.pr_failed", "branch": branch, "error": format!("{e:#}"),
                    }),
                )
                .await
                {
                    state.publish_events(&[ev]).await;
                }
            }
        }
        Up::ArtifactReady { session_id, what, key, error } => {
            // Surface it on the transcript: a client polling the download
            // endpoint learns from the event stream when to retry.
            let payload = match &error {
                Some(e) => serde_json::json!({
                    "type": "session.artifact_failed", "what": what.as_str(), "error": e,
                }),
                None => serde_json::json!({
                    "type": "session.artifact_ready", "what": what.as_str(), "key": key,
                }),
            };
            if let Ok(ev) =
                db::insert_platform_event(&state.pool, session_id, EventKind::Session, payload).await
            {
                state.publish_events(&[ev]).await;
            }
        }
        Up::RequestUpload { session_id, key, content_type: _ } => {
            // Keys are controld-shaped, but the request arrives from a
            // worker: refuse anything outside this session's prefix so a
            // compromised worker cannot mint a URL for another tenant's
            // objects.
            let prefix = format!("sessions/{session_id}/");
            let url = if !key.starts_with(&prefix) || key.contains("..") {
                tracing::warn!(%session_id, %key, "worker asked for an out-of-scope upload key");
                None
            } else {
                state.blobs.as_ref().map(|b| b.presign_put(&key))
            };
            let _ = send_to_worker(state, Some(worker_id), Down::UploadUrl {
                session_id,
                key,
                url,
            });
        }
        Up::RequestSnapshotUrls { snapshot_id, layer, first_part, count } => {
            crate::snapshots::on_request_urls(state, worker_id, snapshot_id, layer, first_part, count).await?;
        }
        Up::SnapshotLayerDone { snapshot_id, layer, parts, plain_bytes, stored_bytes, sha256 } => {
            let done = crate::snapshots::LayerDone { parts, plain_bytes, stored_bytes, sha256 };
            crate::snapshots::on_layer_done(state, worker_id, snapshot_id, layer, done).await?;
        }
        Up::SnapshotState { snapshot_id, machine_id, state: progress, consistency, error, fingerprint, reused } => {
            let report =
                crate::snapshots::Report { snapshot_id, machine_id, progress, consistency, error, fingerprint, reused };
            crate::snapshots::on_state(state, worker_id, report).await?;
        }
        Up::RequestSnapshotGet { snapshot_id, layer } => {
            crate::snapshots::on_request_get(state, worker_id, snapshot_id, layer).await?;
        }
        Up::MachineState { machine_id, generation, state: next, error, volume_existed, reason } => {
            let applied = db::machines::apply_report(
                &state.pool,
                machine_id,
                worker_id,
                generation as i64,
                next,
                error.as_deref(),
                reason.as_deref(),
                volume_existed,
            )
            .await?;
            match applied {
                Some(row) => {
                    tracing::info!(machine = %machine_id, state = %row.state, generation, "machine state");
                    match next {
                        puku_cloud_proto::machine::MachineState::Running => {
                            db::machines::open_run(&state.pool, &row, worker_id).await?;
                            crate::snapshots::reap_stale_copy(state, &row, worker_id).await?;
                        }
                        puku_cloud_proto::machine::MachineState::Stopped
                        | puku_cloud_proto::machine::MachineState::Failed => {
                            db::machines::close_runs(&state.pool, machine_id).await?
                        }
                        _ => {}
                    }
                }
                None => tracing::debug!(machine = %machine_id, generation, "stale machine report ignored"),
            }
        }
    }
    Ok(())
}

/// Park a session whose org has spent past its monthly cap.
///
/// The create-time `check_quota` only gates *starting* a session; without
/// this a single long scheduled run bills straight through the cap. Parking
/// (not killing) keeps the volumes, so the work is resumable once the cap
/// is raised or the month rolls over.
async fn enforce_budget(state: &AppState, session_id: Uuid) {
    let Ok(Some(session)) = db::get_session(&state.pool, session_id).await else { return };
    if session.session_state().is_terminal() || session.session_state() == SessionState::Stopping {
        return;
    }
    let (spent, cap) = match (
        db::month_spend(&state.pool, session.org_id).await,
        db::monthly_cap(&state.pool, session.org_id).await,
    ) {
        (Ok(spent), Ok(cap)) => (spent, cap),
        _ => return, // a metering hiccup must not kill a running session
    };
    if spent < cap {
        return;
    }
    tracing::warn!(
        session = %session_id, org = %session.org_id, spent, cap,
        "monthly budget exhausted mid-run; parking session"
    );
    let reason = format!("monthly budget reached (${spent:.2}/${cap:.2}) — session parked");
    if let Ok((_, Some(ev))) =
        db::transition(&state.pool, session_id, SessionState::Stopping, None).await
    {
        state.publish_events(&[ev]).await;
    }
    if let Ok(ev) = db::insert_platform_event(
        &state.pool,
        session_id,
        EventKind::Session,
        serde_json::json!({
            "type": "session.budget_exceeded",
            "message": reason,
            "spent_usd": spent,
            "cap_usd": cap,
        }),
    )
    .await
    {
        state.publish_events(&[ev]).await;
    }
    let _ = send_to_worker(
        state,
        session.worker_id,
        Down::StopSession { session_id, mode: StopMode::Park },
    );
}

/// Record a pushed branch and open a pull request for it.
async fn open_pr_for(state: &AppState, session_id: Uuid, branch: &str) -> anyhow::Result<()> {
    let session = db::get_session(&state.pool, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let repo = session.repo.clone().ok_or_else(|| anyhow::anyhow!("session has no repo"))?;
    sqlx::query("UPDATE sessions SET branch_pushed = $2 WHERE id = $1")
        .bind(session_id)
        .bind(branch)
        .execute(&state.pool)
        .await?;

    let Some(app) = &*state.github else {
        // A static PAT can push but this platform can't open the PR with
        // it; the branch is still on the remote, so say where it is.
        let ev = db::insert_platform_event(
            &state.pool,
            session_id,
            EventKind::Session,
            serde_json::json!({
                "type": "session.branch_pushed",
                "branch": branch,
                "note": "no GitHub App configured; open the pull request manually",
            }),
        )
        .await?;
        state.publish_events(&[ev]).await;
        return Ok(());
    };

    let title = if session.title.trim().is_empty() {
        format!("puku: session {}", &session_id.to_string()[..8])
    } else {
        session.title.clone()
    };
    let body = format!(
        "Opened by [puku-agent-cloud](https://github.com/sagoresarker/puku-agent-cloud) \
         session `{session_id}`.\n\n**Task**\n\n{}\n",
        session.prompt
    );
    let url = app
        .open_pull_request(&repo, branch, session.branch.as_deref(), &title, &body)
        .await?;
    sqlx::query("UPDATE sessions SET pr_url = $2 WHERE id = $1")
        .bind(session_id)
        .bind(&url)
        .execute(&state.pool)
        .await?;
    let ev = db::insert_platform_event(
        &state.pool,
        session_id,
        EventKind::Session,
        serde_json::json!({"type": "session.pr_opened", "branch": branch, "url": url}),
    )
    .await?;
    state.publish_events(&[ev]).await;
    tracing::info!(%session_id, %branch, %url, "pull request opened");
    Ok(())
}

/// Push a Down frame to the worker that owns a session.
pub fn send_to_worker(state: &AppState, worker_id: Option<Uuid>, frame: Down) -> anyhow::Result<()> {
    let worker_id = worker_id.ok_or_else(|| anyhow::anyhow!("session has no worker"))?;
    let handle = state
        .workers
        .get(worker_id)
        .ok_or_else(|| anyhow::anyhow!("worker offline"))?;
    if !handle.send(frame) {
        anyhow::bail!("worker channel closed");
    }
    Ok(())
}

/// True when the session is in a state where the guest can accept input.
pub fn accepts_input(state: SessionState) -> bool {
    matches!(state, SessionState::Running | SessionState::WaitingInput)
}

#[cfg(test)]
pub(crate) fn test_handle(id: Uuid, cap: u32, engines: Vec<Engine>) -> WorkerHandle {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    WorkerHandle {
        worker_id: id,
        name: "t".into(),
        capacity_slots: cap,
        used_slots: 0,
        draining: false,
        sandboxes: None,
        engines,
        features: Vec::new(),
        host: HostReport::default(),
        token_hash: Vec::new(),
        tx,
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    fn reg(cap: u32) -> (WorkerRegistry, Uuid) {
        let r = WorkerRegistry::new();
        let id = Uuid::new_v4();
        r.insert(test_handle(id, cap, vec![Engine::Libkrun]));
        (r, id)
    }

    fn any() -> Placement {
        Placement::default()
    }

    /// Regression: `used_slots` was only refreshed by the heartbeat, so a
    /// dispatch loop handed every queued session to the same worker before
    /// the first heartbeat landed. Observed as six microVMs on a two-slot
    /// worker.
    #[test]
    fn pick_claims_the_slot_so_a_burst_cannot_oversubscribe() {
        let (r, _) = reg(2);
        assert!(r.pick(&any()).is_some(), "first slot");
        assert!(r.pick(&any()).is_some(), "second slot");
        assert!(r.pick(&any()).is_none(), "third must be refused without a heartbeat");
    }

    #[test]
    fn a_heartbeat_reconciles_the_count_downward() {
        let (r, id) = reg(2);
        r.pick(&any());
        r.pick(&any());
        assert!(r.pick(&any()).is_none());
        // Both finished; the worker says so.
        r.note_slots(id, 0);
        assert!(r.pick(&any()).is_some(), "capacity is usable again after a heartbeat");
    }

    #[test]
    fn a_draining_worker_is_never_picked() {
        let (r, id) = reg(4);
        if let Some(w) = r.inner.lock().unwrap().get_mut(&id) { w.draining = true; }
        assert!(r.pick(&any()).is_none());
    }
}

#[cfg(test)]
mod engine_placement_tests {
    use super::*;

    /// The rule the whole engine field rests on: a libkrun-only worker never
    /// receives a Cloud Hypervisor spec, because it would boot libkrun.
    #[test]
    fn a_worker_is_only_given_engines_it_advertised() {
        let r = WorkerRegistry::new();
        let krun = Uuid::new_v4();
        r.insert(test_handle(krun, 4, vec![Engine::Libkrun]));
        let ch = Placement { engine: Engine::CloudHypervisor, ..Placement::default() };
        assert!(r.pick(&ch).is_none());

        let both = Uuid::new_v4();
        r.insert(test_handle(both, 4, vec![Engine::Libkrun, Engine::CloudHypervisor]));
        assert_eq!(r.pick(&ch).unwrap().worker_id, both);
        assert!(r.any_runs(Engine::CloudHypervisor));
    }

    /// A worker that sent no engines predates them.
    #[test]
    fn an_older_worker_is_libkrun_only() {
        assert_eq!(effective_engines(&[]), vec![Engine::Libkrun]);
        assert_eq!(
            effective_engines(&[Engine::CloudHypervisor, Engine::Unsupported, Engine::CloudHypervisor]),
            vec![Engine::CloudHypervisor]
        );
    }

    /// The volumes are on one host. A less-loaded worker elsewhere is not an
    /// alternative, it is an empty disk.
    #[test]
    fn a_pinned_placement_goes_to_its_worker_or_nowhere() {
        let r = WorkerRegistry::new();
        let home = Uuid::new_v4();
        let idle = Uuid::new_v4();
        r.insert(test_handle(home, 4, vec![Engine::Libkrun]));
        r.insert(test_handle(idle, 4, vec![Engine::Libkrun]));
        r.note_slots(home, 3);
        let p = Placement { engine: Engine::Libkrun, pinned: Some(home), ..Placement::default() };
        assert_eq!(r.pick(&p).unwrap().worker_id, home);
        assert!(r.pick(&p).is_none(), "full is full; the idle worker has no volumes");

        let gone = Placement { engine: Engine::Libkrun, pinned: Some(Uuid::new_v4()), ..Placement::default() };
        assert!(r.pick(&gone).is_none());
    }

    /// A machine goes only to a worker that can run machines at all, and
    /// takes as many slots as its memory needs.
    #[test]
    fn machines_need_the_feature_and_their_slots() {
        let r = WorkerRegistry::new();
        let plain = Uuid::new_v4();
        r.insert(test_handle(plain, 4, vec![Engine::Libkrun]));
        let p = Placement {
            engine: Engine::Libkrun,
            features: &[puku_cloud_proto::worker_proto::FEATURE_MACHINES],
            slots: 2,
            ..Placement::default()
        };
        assert!(r.pick(&p).is_none(), "no machines feature");

        let capable = Uuid::new_v4();
        let mut h = test_handle(capable, 3, vec![Engine::Libkrun]);
        h.features = vec![puku_cloud_proto::worker_proto::FEATURE_MACHINES.to_string()];
        r.insert(h);
        assert_eq!(r.pick(&p).unwrap().worker_id, capable);
        assert!(r.pick(&p).is_none(), "2 + 2 slots do not fit in 3");
        assert!(r.pick(&Placement::default()).is_some(), "but a 1-slot session still does");
    }

    /// Draining stops new work, not a resume onto volumes only it has.
    #[test]
    fn a_draining_worker_still_takes_its_own_resume() {
        let r = WorkerRegistry::new();
        let id = Uuid::new_v4();
        r.insert(test_handle(id, 4, vec![Engine::Libkrun]));
        r.set_draining(id, true);
        assert!(r.pick(&Placement::default()).is_none());
        let p = Placement { engine: Engine::Libkrun, pinned: Some(id), ..Placement::default() };
        assert!(r.pick(&p).is_some());
    }
}

#[cfg(test)]
mod live_capacity_tests {
    use super::*;

    fn one_worker(cap: u32) -> (WorkerRegistry, Uuid) {
        let r = WorkerRegistry::new();
        let id = Uuid::new_v4();
        r.insert(test_handle(id, cap, vec![Engine::Libkrun]));
        (r, id)
    }

    fn any() -> Placement {
        Placement::default()
    }

    /// A worker that has filled its memory reports a smaller capacity, and
    /// the control plane must act on it rather than keep assigning against
    /// the number it registered with.
    #[test]
    fn a_shrinking_worker_stops_being_picked() {
        let (r, id) = one_worker(11);
        for _ in 0..6 {
            assert!(r.pick(&any()).is_some());
        }
        // Host is now tight: it can hold what it has and no more.
        r.note_capacity(id, 6);
        assert!(r.pick(&any()).is_none(), "a full host must not be assigned more work");
    }

    /// And it recovers on its own once sessions finish.
    #[test]
    fn capacity_recovers_without_a_reconnect() {
        let (r, id) = one_worker(2);
        assert!(r.pick(&any()).is_some());
        assert!(r.pick(&any()).is_some());
        assert!(r.pick(&any()).is_none());
        r.note_capacity(id, 8);
        assert!(r.pick(&any()).is_some(), "freed memory should be usable again");
    }
}

#[cfg(test)]
mod diagnose_tests {
    use super::*;
    use puku_cloud_proto::worker_proto::StagedImage;

    fn machines_worker(r: &WorkerRegistry, cap: u32, host: HostReport) -> Uuid {
        let id = Uuid::new_v4();
        let mut h = test_handle(id, cap, vec![Engine::Libkrun, Engine::CloudHypervisor]);
        h.features = vec![FEATURE_MACHINES.to_string()];
        h.host = host;
        r.insert(h);
        id
    }

    fn machine(slots: u32) -> Placement {
        Placement { engine: Engine::Libkrun, features: &[FEATURE_MACHINES], slots, ..Placement::default() }
    }

    #[test]
    fn an_empty_fleet_has_no_workers() {
        assert_eq!(WorkerRegistry::new().diagnose(&machine(1)), Err(Unplaceable::NoWorkers));
    }

    /// "Full right now" on the worker that runs machines beats "does not run
    /// machines" on the one that does not: it is the one worth waiting for.
    #[test]
    fn the_nearest_worker_decides_the_reason() {
        let r = WorkerRegistry::new();
        r.insert(test_handle(Uuid::new_v4(), 8, vec![Engine::Libkrun]));
        let busy = machines_worker(&r, 2, HostReport::default());
        r.note_slots(busy, 2);
        assert_eq!(
            r.diagnose(&machine(1)),
            Err(Unplaceable::CapacityFull { requested_slots: 1, best_free_slots: 0, workers: 1 })
        );
        r.note_slots(busy, 0);
        assert_eq!(r.diagnose(&machine(1)), Ok(()));
        assert_eq!(r.diagnose(&machine(1)), Ok(()), "diagnose claims nothing");
        assert!(r.pick(&machine(2)).is_some());
    }

    #[test]
    fn too_large_needs_a_reported_ceiling() {
        let r = WorkerRegistry::new();
        machines_worker(&r, 4, HostReport::default());
        assert!(
            matches!(r.diagnose(&machine(8)), Err(Unplaceable::CapacityFull { .. })),
            "a worker that reports no ceiling is only ever full"
        );

        let r = WorkerRegistry::new();
        machines_worker(&r, 4, HostReport { max_slots: Some(4), cores: Some(8), ..Default::default() });
        assert!(matches!(r.diagnose(&machine(8)), Err(Unplaceable::TooLarge { max_slots: Some(4), .. })));
        let wide = Placement { cpus: 16, ..machine(1) };
        assert!(matches!(r.diagnose(&wide), Err(Unplaceable::TooLarge { max_cores: Some(8), .. })));
    }

    #[test]
    fn a_cloud_hypervisor_machine_needs_its_image_staged() {
        let r = WorkerRegistry::new();
        let img = StagedImage {
            engine: Engine::CloudHypervisor,
            key: "pukubot_latest".into(),
            image: None,
            digest: None,
            size_mib: None,
        };
        machines_worker(&r, 8, HostReport { images: Some(vec![img]), ..Default::default() });
        let want = |key: &str| Placement {
            engine: Engine::CloudHypervisor,
            image_key: Some(key.into()),
            ..machine(1)
        };
        assert_eq!(r.diagnose(&want("pukubot_latest")), Ok(()));
        assert_eq!(
            r.diagnose(&want("other_latest")),
            Err(Unplaceable::ImageNotStaged { image_key: "other_latest".into() })
        );

        // A worker that reports no image list is never refused for one.
        let r = WorkerRegistry::new();
        machines_worker(&r, 8, HostReport::default());
        assert_eq!(r.diagnose(&want("other_latest")), Ok(()));
    }

    #[test]
    fn a_pinned_placement_is_about_its_own_worker() {
        let r = WorkerRegistry::new();
        let home = machines_worker(&r, 8, HostReport::default());
        let gone = Uuid::new_v4();
        let p = Placement { pinned: Some(gone), ..machine(1) };
        assert_eq!(r.diagnose(&p), Err(Unplaceable::VolumeHostOffline { worker_id: gone }));

        let p = Placement { pinned: Some(home), ..machine(1) };
        r.note_slots(home, 8);
        assert_eq!(
            r.diagnose(&p),
            Err(Unplaceable::VolumeHostFull { worker_id: home, requested_slots: 1, free_slots: 0 })
        );
    }

    #[test]
    fn disk_draining_features_and_engines_have_their_own_reasons() {
        let r = WorkerRegistry::new();
        let id = machines_worker(&r, 8, HostReport { disk_free_mib: Some(100), ..Default::default() });
        let needs_disk = Placement { disk_mib: 1024, ..machine(1) };
        assert_eq!(
            r.diagnose(&needs_disk),
            Err(Unplaceable::InsufficientDisk { needed_mib: 1024, best_free_mib: Some(100) })
        );
        r.set_draining(id, true);
        assert_eq!(r.diagnose(&machine(1)), Err(Unplaceable::AllDraining));

        let r = WorkerRegistry::new();
        r.insert(test_handle(Uuid::new_v4(), 8, vec![Engine::Libkrun]));
        assert_eq!(r.diagnose(&machine(1)).unwrap_err().reason(), "machines_unsupported");
        let ch = Placement { engine: Engine::CloudHypervisor, ..machine(1) };
        assert_eq!(r.diagnose(&ch), Err(Unplaceable::EngineUnavailable { engine: Engine::CloudHypervisor }));
    }
}
