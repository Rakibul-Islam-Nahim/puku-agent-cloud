//! The persistent dial-out link to controld, and routing of Down frames to
//! session actors.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::worker_proto::{Down, HostReport, StopMode, Up};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::session_actor::{InputCmd, SessionActor};
use crate::vm::Backends;
use crate::Args;

/// Capacity for a host that cannot be measured -- anything but Linux, where
/// /proc/meminfo is what sizes it -- when the operator pinned no number.
/// Zero refused every VM, which on a development laptop, the usual such
/// host, looked like a full fleet; a handful is what one holds.
const UNMEASURED_CAPACITY: u32 = 4;

/// Commands routed to a running session actor.
#[derive(Clone)]
pub struct SessionHandle {
    pub input_tx: mpsc::UnboundedSender<InputCmd>,
}

#[derive(Clone, Default)]
pub struct SessionMap {
    inner: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
}

impl SessionMap {
    pub fn insert(&self, id: Uuid, h: SessionHandle) {
        self.inner.lock().unwrap().insert(id, h);
    }
    pub fn remove(&self, id: Uuid) {
        self.inner.lock().unwrap().remove(&id);
    }
    pub fn get(&self, id: Uuid) -> Option<SessionHandle> {
        self.inner.lock().unwrap().get(&id).cloned()
    }
    pub fn ids(&self) -> Vec<Uuid> {
        self.inner.lock().unwrap().keys().copied().collect()
    }
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

pub struct Link {
    args: Args,
    /// The engines this worker runs. Each session actor gets the one its
    /// spec names.
    backends: Backends,
    /// Machines (docs/MACHINES-API.md) on this worker.
    machines: crate::machines::Machines,
    /// The pool of data sockets machine traffic rides on.
    data: Arc<crate::datalink::DataLink>,
    sessions: SessionMap,
    /// Shared by every session actor: presigned-URL requests and the PUTs
    /// that follow them ride the one control link.
    uploader: crate::uploader::Uploader,
    /// Brokers the fresh push token each finishing session asks for.
    git_tokens: crate::gitpush::GitTokens,
    up_tx: mpsc::UnboundedSender<Up>,
    // Held here so the receiver survives reconnects; taken by run_once.
    up_rx: Mutex<Option<mpsc::UnboundedReceiver<Up>>>,
}

impl Link {
    pub fn new(args: Args, backends: Backends) -> Self {
        let (up_tx, up_rx) = mpsc::unbounded_channel();
        let machines = crate::machines::Machines::new(
            &args.state_dir,
            backends.clone(),
            up_tx.clone(),
            args.egress_allow(),
            args.multi_tenant,
            crate::snapshot::Settings {
                zstd_level: args.snapshot_zstd_level,
                concurrency: args.snapshot_concurrency.max(1),
            },
        );
        let data = crate::datalink::DataLink::new(
            args.data_url(),
            args.worker_name.clone(),
            args.worker_token.clone(),
            machines.clone(),
        );
        Link {
            args,
            backends,
            machines,
            data,
            sessions: SessionMap::default(),
            uploader: crate::uploader::Uploader::new(up_tx.clone()),
            git_tokens: crate::gitpush::GitTokens::new(up_tx.clone()),
            up_tx,
            up_rx: Mutex::new(Some(up_rx)),
        }
    }

    /// Reattach to machines whose VMs survived a restart, then start keeping
    /// data sockets open. Before registering, so Register lists them.
    pub async fn start_machines(&self) {
        self.machines.reconcile_from_disk().await;
        self.data.spawn();
    }

    /// Reattach to sessions whose VMs survived a workerd restart: every
    /// session dir with a persisted spec gets a recovery actor. Dead
    /// sandboxes are reported failed by the actor itself.
    pub fn reconcile_from_disk(&self) {
        let sessions_root = self.args.state_dir.join("sessions");
        let Ok(entries) = std::fs::read_dir(&sessions_root) else { return };
        for entry in entries.flatten() {
            let spec_path = entry.path().join("spec.json");
            let Ok(bytes) = std::fs::read(&spec_path) else { continue };
            match serde_json::from_slice::<puku_cloud_proto::session::SessionSpec>(&bytes) {
                Ok(spec) => {
                    tracing::info!(session = %spec.session_id, "reconciling session from disk");
                    self.spawn_actor(spec, true);
                }
                Err(e) => {
                    tracing::warn!(path = %spec_path.display(), error = %e, "bad spec.json");
                }
            }
        }
    }

    fn spawn_actor(&self, spec: puku_cloud_proto::session::SessionSpec, recovered: bool) {
        let session_id = spec.session_id;
        if self.sessions.get(session_id).is_some() {
            tracing::warn!(%session_id, "duplicate assignment ignored");
            return;
        }
        // controld only assigns engines this worker advertised, so this is a
        // spec.json from before an operator switched an engine off, or a
        // newer control plane naming an engine this build does not know.
        // Refuse it by name; booting it on another engine would put the
        // session on volumes laid out for a different guest.
        let Some(backend) = self.backends.get(spec.engine) else {
            let msg = format!("this worker does not run the {} engine", spec.engine);
            tracing::warn!(%session_id, engine = %spec.engine, "{msg}");
            let _ = self.up_tx.send(Up::SessionState {
                session_id,
                state: puku_cloud_proto::session::SessionState::Failed,
                error: Some(msg),
                puku_session_id: None,
            });
            if recovered {
                let base = crate::session_actor::session_base_dir(&self.args.state_dir, session_id);
                let _ = std::fs::remove_file(base.join("spec.json"));
            }
            return;
        };
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        self.sessions.insert(session_id, SessionHandle { input_tx });
        let actor = SessionActor {
            spec,
            state_dir: self.args.state_dir.clone(),
            up_tx: self.up_tx.clone(),
            sessions: self.sessions.clone(),
            runner_cmd: self.args.runner_cmd.clone(),
            recovered,
            multi_tenant: self.args.multi_tenant,
            egress_allow: self.args.egress_allow(),
            secret_hosts: self.args.secret_hosts(),
            secret_env_injection: self.args.secret_env_injection,
            uploader: self.uploader.clone(),
            git_tokens: self.git_tokens.clone(),
            backend,
        };
        // Hub-per-task, not `configure_scope`. Scope lives on the *thread's*
        // hub, and tokio multiplexes many session actors onto each worker
        // thread -- tagging there would stamp this session's id onto
        // whichever unrelated actor polled next. `bind_hub` is what makes
        // `Hub::current()` resolve to this actor's own hub for the whole of
        // every poll, including inside the panic hook.
        let hub = sentry::Hub::new_from_top(sentry::Hub::current());
        hub.configure_scope(|scope| {
            scope.set_tag("session_id", session_id.to_string());
            scope.set_tag("recovered", recovered.to_string());
            scope.set_transaction(Some("session_actor"));
        });
        tokio::spawn(sentry::SentryFutureExt::bind_hub(actor.run(input_rx), hub));
    }

    /// One connection lifetime: connect, register, pump frames until the
    /// socket drops. Session actors keep running across reconnects; their
    /// Up frames buffer in the channel while the link is down.
    pub async fn run_once(&self) -> Result<()> {
        let (ws, _) = connect_async(&self.args.controld_url)
            .await
            .context("connecting to controld")?;
        let (mut sink, mut stream) = ws.split();

        if self.args.capacity_slots == 0 && crate::hostcap::probe().is_none() {
            tracing::warn!(
                slots = UNMEASURED_CAPACITY,
                "cannot size this host from its memory; offering a fixed capacity (set PUKU_CAPACITY_SLOTS to choose one)"
            );
        }
        let register = Up::Register {
            worker_name: self.args.worker_name.clone(),
            auth_token: self.args.worker_token.clone(),
            capacity_slots: self.live_capacity(0).unwrap_or_else(|| self.fixed_capacity()),
            msb_version: self.backends.version_string(),
            running_sessions: self.sessions.ids(),
            on_disk_sessions: self.on_disk_sessions(),
            engines: self.backends.engines(),
            features: vec![
                puku_cloud_proto::worker_proto::FEATURE_MACHINES.to_string(),
                puku_cloud_proto::worker_proto::FEATURE_LEASE.to_string(),
                puku_cloud_proto::snapshot::FEATURE_SNAPSHOTS.to_string(),
            ],
            running_machines: self.machines.running_ids(),
            on_disk_machines: self.machines.on_disk_ids(),
            host: Some(self.host_report()),
        };
        sink.send(Message::Text(serde_json::to_string(&register)?.into()))
            .await?;

        // Await the ack before doing anything else.
        let _resume_cursors = loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<Down>(&t) {
                    Ok(Down::RegisterAck { worker_id, resume_cursors, reapable, reapable_machines }) => {
                        tracing::info!(%worker_id, "registered with controld");
                        // Reconcile: anything the control plane has already
                        // archived is dead weight on this disk.
                        for id in reapable {
                            self.reap_session(id);
                        }
                        // Machines destroyed (or unknown) upstream: their
                        // volumes can go. A running one is left alone -- the
                        // control plane stops it explicitly if it should not
                        // be running.
                        for id in reapable_machines {
                            if self.machines.get(id).is_none() {
                                let machines = self.machines.clone();
                                tokio::spawn(async move { machines.destroy(id, None).await });
                            }
                        }
                        break resume_cursors;
                    }
                    Ok(_) => continue,
                    Err(e) => anyhow::bail!("bad frame during registration: {e}"),
                },
                Some(Ok(_)) => continue,
                _ => anyhow::bail!("connection closed during registration"),
            }
        };

        let mut up_rx = self
            .up_rx
            .lock()
            .unwrap()
            .take()
            .expect("up_rx missing: run_once called concurrently");

        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        // Liveness, separate from the inventory heartbeat. Missing ticks
        // only ever costs this host its new placements and, much later, its
        // sessions -- never its running VMs: the worker does not stop
        // anything on its own when the link is down.
        let mut lease = tokio::time::interval(Duration::from_secs(1));
        lease.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    _ = lease.tick() => {
                        sink.send(Message::Text(serde_json::to_string(&Up::LeaseRenew)?.into())).await?;
                    }
                    _ = heartbeat.tick() => {
                        // Ground truth from the host, not from our own
                        // bookkeeping: the point of reporting this is to
                        // expose where the two disagree.
                        let used = self.sessions.len() as u32 + self.machines.slots();
                        let hb = Up::Heartbeat {
                            used_slots: used,
                            capacity_slots: self.live_capacity(used),
                            sandboxes: self.backends.inventory().await,
                            host: Some(self.host_report()),
                        };
                        sink.send(Message::Text(serde_json::to_string(&hb)?.into())).await?;
                    }
                    up = up_rx.recv() => {
                        let Some(frame) = up else { anyhow::bail!("up channel closed") };
                        sink.send(Message::Text(serde_json::to_string(&frame)?.into())).await?;
                    }
                    down = stream.next() => {
                        let text = match down {
                            Some(Ok(Message::Text(t))) => t,
                            Some(Ok(Message::Close(_))) | None => anyhow::bail!("controld closed the link"),
                            Some(Ok(_)) => continue,
                            Some(Err(e)) => return Err(e.into()),
                        };
                        match serde_json::from_str::<Down>(&text) {
                            Ok(frame) => self.handle_down(frame),
                            Err(e) => tracing::warn!(error = %e, "bad down frame"),
                        }
                    }
                }
            }
        }
        .await;

        // Put the receiver back for the next connection attempt.
        *self.up_rx.lock().unwrap() = Some(up_rx);
        result
    }

    fn handle_down(&self, frame: Down) {
        match frame {
            Down::RegisterAck { .. } => {}
            Down::AssignSession { spec } => {
                self.spawn_actor(spec, false);
            }
            Down::DeliverInput { session_id, stream_json_line } => {
                self.route(session_id, InputCmd::Line(stream_json_line));
            }
            Down::Interrupt { session_id } => {
                self.route(session_id, InputCmd::Interrupt);
            }
            Down::ReapSession { session_id } => {
                self.reap_session(session_id);
            }
            Down::UploadUrl { key, url, .. } => {
                self.uploader.resolve(&key, url);
            }
            Down::GitToken { session_id, token } => {
                self.git_tokens.resolve(session_id, token);
            }
            Down::CollectArtifact { session_id, what, key } => {
                self.collect_artifact(session_id, what, key);
            }
            Down::StopSession { session_id, mode } => {
                let cmd = match mode {
                    StopMode::Park => InputCmd::Park,
                    StopMode::Kill => InputCmd::Kill,
                };
                self.route(session_id, cmd);
            }
            // Machine lifecycle runs off the control loop: a boot pulls an
            // image and waits on a VM, and must not stall every session's
            // events behind it.
            Down::AssignMachine { spec } => {
                let machines = self.machines.clone();
                tokio::spawn(async move { machines.assign(spec).await });
            }
            Down::StopMachine { machine_id, generation, snapshot } => {
                let machines = self.machines.clone();
                tokio::spawn(async move { machines.stop(machine_id, generation, snapshot).await });
            }
            Down::DestroyMachine { machine_id, final_snapshot } => {
                let machines = self.machines.clone();
                tokio::spawn(async move { machines.destroy(machine_id, final_snapshot).await });
            }
            Down::OpenDataSockets { count } => {
                self.data.dial(count.min(8) as usize);
            }
            // Registered here, synchronously, so a stop or destroy that
            // follows on the link finds the capture already in flight.
            Down::SnapshotMachine { order } => self.machines.snapshot(order),
            Down::SnapshotUrls { snapshot_id, layer, first_part, urls, error } => {
                self.machines.snapshots().resolve_urls(snapshot_id, layer, first_part, urls, error);
            }
            Down::SnapshotGetUrl { snapshot_id, layer, url } => {
                self.machines.snapshots().resolve_get(snapshot_id, layer, url);
            }
            Down::ReapMachine { machine_id, below_generation } => {
                let machines = self.machines.clone();
                tokio::spawn(async move { machines.reap(machine_id, below_generation).await });
            }
        }
    }

    /// Package a session directory and ship it to object storage.
    ///
    /// Runs off the control loop: taring a large workspace takes seconds to
    /// minutes and must not stall the link that carries every other
    /// session's events.
    /// Capacity as the host sees it now, or None when it cannot be read or
    /// the operator pinned a number.
    ///
    /// Recomputed every heartbeat on purpose: memory is the binding
    /// constraint and it moves. A host filling up advertises less until it
    /// meets `used_slots`, so the control plane stops assigning rather than
    /// over-committing, and it recovers on its own as sessions finish.
    fn live_capacity(&self, used: u32) -> Option<u32> {
        if self.args.capacity_slots != 0 {
            return None; // pinned by the operator; do not second-guess it
        }
        let host = self.host()?;
        Some(crate::hostcap::advertise(&host, used, crate::hostcap::VM_CPUS, crate::hostcap::VM_MEMORY_MIB))
    }

    /// The number `PUKU_CAPACITY_SLOTS` pins; left at 0 ("size it from the
    /// host") on a host that cannot be sized, a small default rather than a
    /// capacity of nothing.
    fn fixed_capacity(&self) -> u32 {
        match self.args.capacity_slots {
            0 => UNMEASURED_CAPACITY,
            n => n,
        }
    }

    /// The host as capacity counts it: cores stretched by
    /// `PUKU_CPU_OVERCOMMIT`, because idle desktops leave most of theirs
    /// unused; memory, which cannot be overcommitted, as it is.
    fn host(&self) -> Option<crate::hostcap::Host> {
        let h = crate::hostcap::probe()?;
        let factor = self.args.cpu_overcommit.clamp(1.0, 16.0);
        Some(crate::hostcap::Host { cores: (h.cores as f64 * factor) as u32, ..h })
    }

    /// The host as placement needs to see it: the most it could ever hold,
    /// its free disk, and the images staged here (see `HostReport`).
    fn host_report(&self) -> HostReport {
        let host = self.host();
        let (disk_free_mib, disk_total_mib) = crate::hostcap::disk(&self.args.state_dir).unzip();
        HostReport {
            cores: host.map(|h| h.cores),
            mem_total_mib: host.map(|h| h.mem_total_mib),
            // A number the operator pinned is the ceiling as well.
            max_slots: if self.args.capacity_slots != 0 {
                Some(self.args.capacity_slots)
            } else {
                host.map(|h| crate::hostcap::ceiling(&h, crate::hostcap::VM_CPUS, crate::hostcap::VM_MEMORY_MIB))
            },
            disk_free_mib,
            disk_total_mib,
            images: self.backends.staged_images(),
        }
    }

    /// Every session id this worker still holds a directory for.
    fn on_disk_sessions(&self) -> Vec<Uuid> {
        let root = self.args.state_dir.join("sessions");
        let Ok(rd) = std::fs::read_dir(&root) else { return Vec::new() };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().and_then(|n| Uuid::parse_str(n).ok()))
            .collect()
    }

    /// Drop a finished session's volumes.
    ///
    /// Nothing did this before, so `<state_dir>/sessions/<id>` accumulated
    /// forever -- each one holding a whole `/workspace` (a cloned repo, a
    /// venv, whatever the agent produced) and a full copy of every skill pack
    /// unpacked into its HOME. A box doing document work leaks on the order
    /// of tens of megabytes per session until the disk fills and VMs stop
    /// booting.
    ///
    /// Only ever driven by the control plane, and only after it has archived
    /// the transcript, so this cannot race a resume or an artifact fetch.
    fn reap_session(&self, session_id: Uuid) {
        // A live actor means the control plane and this worker disagree about
        // whether the session is finished. Believe the running VM.
        if self.sessions.get(session_id).is_some() {
            tracing::warn!(%session_id, "refusing to reap a session that is still running here");
            return;
        }
        let dir = self.args.state_dir.join("sessions").join(session_id.to_string());
        if !dir.exists() {
            return;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(%session_id, path = %dir.display(), "reaped session volumes"),
            Err(e) => tracing::warn!(%session_id, error = %e, "reaping session volumes failed"),
        }
        // The volumes were only half of it. A sandbox whose teardown was
        // missed -- a boot that failed part-way, a worker killed mid-session
        // -- stays registered with its engine holding its memory, and reaping
        // the directory alone left it there for ever. The directory that
        // said which engine is already gone, so ask every engine: removing
        // a name an engine never had is a harmless error.
        let name = crate::session_actor::sandbox_name_for(session_id);
        for backend in self.backends.all() {
            let name = name.clone();
            tokio::spawn(async move {
                if let Err(e) = backend.remove(&name).await {
                    tracing::debug!(sandbox = %name, engine = %backend.engine(), error = %e, "removing a reaped session's sandbox");
                }
            });
        }
    }

    fn collect_artifact(
        &self,
        session_id: Uuid,
        what: puku_cloud_proto::worker_proto::ArtifactKind,
        key: String,
    ) {
        use puku_cloud_proto::worker_proto::ArtifactKind;
        let base = crate::session_actor::session_base_dir(&self.args.state_dir, session_id);
        let src = match what {
            ArtifactKind::Workspace => base.join("workspace"),
            ArtifactKind::Home => base.join("session").join("home"),
        };
        let uploader = self.uploader.clone();
        let up_tx = self.up_tx.clone();
        tokio::spawn(async move {
            let dest = base.join(format!("artifact-{}.tgz", what.as_str()));
            let error = match crate::uploader::tar_directory(&src, &dest).await {
                Ok(()) => {
                    uploader.upload(session_id, key.clone(), dest, "application/gzip");
                    None
                }
                Err(e) => {
                    tracing::warn!(%session_id, error = format!("{e:#}"), "collecting artifact failed");
                    Some(format!("{e:#}"))
                }
            };
            let _ = up_tx.send(Up::ArtifactReady { session_id, what, key, error });
        });
    }

    fn route(&self, session_id: Uuid, cmd: InputCmd) {
        match self.sessions.get(session_id) {
            Some(h) => {
                let _ = h.input_tx.send(cmd);
            }
            None => tracing::warn!(%session_id, "command for unknown session"),
        }
    }
}

