//! Machines on this worker: boot, stop, destroy, and survive a restart.
//!
//! A machine is a VM plus, optionally, a volume that outlives it. The VM is
//! the engine's business (via [`crate::vm`]); the volume is a host directory
//! under `<state_dir>/machines/<id>/volume`, bind-mounted at the spec's path,
//! which is what lets a stopped machine start again with its files.
//!
//! On-disk markers make a restart recoverable:
//! * `<id>/` exists            -> this worker holds state for the machine
//! * `<id>/spec.json` exists   -> its VM should be running; reattach to it

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use puku_cloud_proto::machine::{slots_for, MachineSpec, MachineState};
use puku_cloud_proto::snapshot::SnapshotOrder;
use puku_cloud_proto::worker_proto::Up;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::snapshot::Snapshots;
use crate::vm::{Backends, BootRefusal, ExecRequest, Mount, Vm, VmSpec};

/// A machine whose VM this worker is running.
pub struct Running {
    pub spec: MachineSpec,
    pub vm: Box<dyn Vm>,
}

impl Running {
    /// Who a command runs as when the caller names nobody.
    pub fn default_user(&self) -> Option<String> {
        self.spec.default_user()
    }
}

#[derive(Clone)]
pub struct Machines {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    backends: Backends,
    up_tx: mpsc::UnboundedSender<Up>,
    egress_allow: Vec<String>,
    multi_tenant: bool,
    snapshots: Snapshots,
    running: Mutex<HashMap<Uuid, Arc<Running>>>,
    /// One lifecycle operation per machine at a time: a stop racing a boot
    /// of the same machine must not interleave.
    locks: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
}

impl Machines {
    pub fn new(
        state_dir: &Path,
        backends: Backends,
        up_tx: mpsc::UnboundedSender<Up>,
        egress_allow: Vec<String>,
        multi_tenant: bool,
        snapshot_settings: crate::snapshot::Settings,
    ) -> Self {
        let root = state_dir.join("machines");
        Machines {
            inner: Arc::new(Inner {
                snapshots: Snapshots::new(root.clone(), up_tx.clone(), snapshot_settings),
                root,
                backends,
                up_tx,
                egress_allow,
                multi_tenant,
                running: Mutex::new(HashMap::new()),
                locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn dir(&self, id: Uuid) -> PathBuf {
        self.inner.root.join(id.to_string())
    }

    fn lock(&self, id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        self.inner.locks.lock().unwrap().entry(id).or_default().clone()
    }

    pub fn get(&self, id: Uuid) -> Option<Arc<Running>> {
        self.inner.running.lock().unwrap().get(&id).cloned()
    }

    pub fn running_ids(&self) -> Vec<Uuid> {
        self.inner.running.lock().unwrap().keys().copied().collect()
    }

    /// Capacity the running machines occupy, in session-sized slots.
    pub fn slots(&self) -> u32 {
        self.inner.running.lock().unwrap().values().map(|r| slots_for(r.spec.cpus as u32, r.spec.memory_mib)).sum()
    }

    /// Every machine this worker holds a directory for.
    pub fn on_disk_ids(&self) -> Vec<Uuid> {
        let Ok(rd) = std::fs::read_dir(&self.inner.root) else { return Vec::new() };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().and_then(|n| Uuid::parse_str(n).ok()))
            .collect()
    }

    fn report(
        &self,
        spec: &MachineSpec,
        state: MachineState,
        error: Option<String>,
        reason: Option<&str>,
        volume_existed: bool,
    ) {
        let _ = self.inner.up_tx.send(Up::MachineState {
            machine_id: spec.machine_id,
            generation: spec.generation,
            state,
            error,
            volume_existed,
            reason: reason.map(str::to_string),
        });
    }

    /// Boot a machine (or confirm it is already running this generation).
    pub async fn assign(&self, spec: MachineSpec) {
        let lock = self.lock(spec.machine_id);
        let _guard = lock.lock().await;
        if let Some(current) = self.get(spec.machine_id) {
            if current.spec.generation == spec.generation {
                // A duplicate assignment: say again what is true.
                self.report(&spec, MachineState::Running, None, None, true);
                return;
            }
            // An older boot is still up; this one supersedes it.
            self.teardown(spec.machine_id).await;
        }
        if spec.restore.is_some() {
            // A capture still reading the old volume finishes before the
            // restore swaps it out from under it.
            self.inner.snapshots.wait_idle(spec.machine_id).await;
            self.report(&spec, MachineState::Restoring, None, None, false);
            if let Err(e) = self.inner.snapshots.restore(&spec, &self.dir(spec.machine_id)).await {
                let msg = format!("{e:#}");
                tracing::warn!(machine = %spec.machine_id, error = %msg, "restoring the machine failed");
                self.report(&spec, MachineState::Failed, Some(msg), Some("restore_failed"), false);
                return;
            }
        } else {
            // A capture of the stopped machine may still be reading its root
            // disk, which this boot is about to write to: wait for that much.
            // What it reads of the volume from here on is live.
            self.inner.snapshots.before_boot(spec.machine_id).await;
        }
        self.report(&spec, MachineState::Booting, None, None, false);
        match self.boot(&spec).await {
            Ok(volume_existed) => {
                tracing::info!(machine = %spec.machine_id, engine = %spec.engine, volume_existed, "machine running");
                self.report(&spec, MachineState::Running, None, None, volume_existed);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let reason = crate::vm::boot_failure_reason(&e);
                tracing::warn!(machine = %spec.machine_id, reason, error = %msg, "machine failed to boot");
                self.teardown(spec.machine_id).await;
                let _ = std::fs::remove_file(self.dir(spec.machine_id).join("spec.json"));
                self.report(&spec, MachineState::Failed, Some(msg), Some(reason), false);
            }
        }
    }

    /// Returns whether the volume already existed.
    async fn boot(&self, spec: &MachineSpec) -> Result<bool> {
        let backend = self.inner.backends.get(spec.engine).ok_or_else(|| {
            BootRefusal::new("engine_unavailable", format!("this worker does not run the {} engine", spec.engine))
        })?;
        let dir = self.dir(spec.machine_id);
        std::fs::create_dir_all(&dir).context("creating the machine directory")?;

        let mut mounts = Vec::new();
        let mut volume_existed = false;
        if let Some(vol) = &spec.volume {
            let host = dir.join("volume");
            volume_existed = host.exists();
            std::fs::create_dir_all(&host).context("creating the machine volume")?;
            #[cfg(unix)]
            if let Err(e) = std::os::unix::fs::chown(&host, Some(vol.uid), Some(vol.uid)) {
                tracing::debug!(error = %e, "chown of the volume skipped");
            }
            mounts.push(Mount { guest: vol.path.clone(), host });
        }

        // A kept root disk belongs to the image it was made on. A different
        // image starts a fresh one: packages installed over one base need not
        // work over another.
        let root_disk = if spec.persist_root {
            let disk = dir.join("root.ext4");
            let marker = dir.join("root.image");
            if std::fs::read_to_string(&marker).ok().as_deref() != Some(spec.image.as_str()) {
                if disk.exists() {
                    tracing::info!(machine = %spec.machine_id, image = %spec.image, "the image changed; starting a fresh root disk");
                }
                let _ = std::fs::remove_file(&disk);
                std::fs::write(&marker, &spec.image).context("recording the root disk's image")?;
            }
            Some(disk)
        } else {
            None
        };

        // A previous boot that died with the worker can leave the engine
        // holding the name; clear it so this create does not collide.
        let _ = backend.remove(&spec.name).await;

        let vm_spec = VmSpec {
            name: spec.name.clone(),
            image: spec.image.clone(),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            mounts,
            labels: vec![
                ("puku.machine".into(), spec.machine_id.to_string()),
                ("puku.managed".into(), "true".into()),
            ],
            max_duration_s: (spec.max_duration_s > 0).then_some(spec.max_duration_s as u64),
            env: spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            secrets: Vec::new(),
            egress_allow: self.inner.egress_allow.clone(),
            multi_tenant: self.inner.multi_tenant,
            ports: spec.expose.clone(),
            root_disk,
            // A desktop runs a browser per screen, and each wants far more
            // /dev/shm than the few hundred MiB a session gets.
            shm_mib: Some((spec.memory_mib / 4).max(512)),
        };
        let vm = backend.create(&vm_spec).await.context("creating the VM")?;

        // Hand the volume root to its owner from inside the guest too. The
        // host-side chown above needs workerd to be root; a worker that is
        // not (development on a laptop) leaves the directory looking
        // root-owned in the guest, and every write as the volume's uid then
        // fails with EACCES. The engines' file sharing keeps ownership set
        // from the guest, so this works either way, and is a no-op where the
        // host chown already did it.
        if let Some(vol) = &spec.volume {
            let owner = format!("{}:{}", vol.uid, vol.uid);
            let out = vm
                .exec(ExecRequest {
                    cmd: "chown".into(),
                    args: vec![owner, vol.path.clone()],
                    user: Some("root".into()),
                    ..Default::default()
                })
                .await;
            if !matches!(&out, Ok(o) if o.success()) {
                tracing::debug!(machine = %spec.machine_id, result = ?out.map(|o| o.code), "in-guest chown of the volume failed");
            }
        }

        if let Some(ep) = &spec.entrypoint {
            // `sh -c '<script>' sh <argv...>` passes argv through as
            // positional parameters, so nothing in it is ever re-parsed by a
            // shell. setsid detaches it from the exec, so it belongs to the
            // VM rather than to a connection this worker might lose.
            let mut args = vec![
                "-c".to_string(),
                "setsid \"$@\" < /dev/null >> /tmp/puku-entrypoint.log 2>&1 &".to_string(),
                "sh".to_string(),
            ];
            args.extend(ep.argv.iter().cloned());
            let out = vm
                .exec(ExecRequest {
                    cmd: "sh".into(),
                    args,
                    user: ep.user.clone().or_else(|| spec.default_user()),
                    env: vm_spec.env.clone(),
                    ..Default::default()
                })
                .await
                .context("starting the entrypoint")?;
            if !out.success() {
                anyhow::bail!(
                    "the entrypoint did not start (exit {}): {}",
                    out.code,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }

        std::fs::write(dir.join("spec.json"), serde_json::to_vec(spec)?)?;
        self.inner
            .running
            .lock()
            .unwrap()
            .insert(spec.machine_id, Arc::new(Running { spec: spec.clone(), vm }));
        Ok(volume_existed)
    }

    /// Stop the VM and forget it, keeping the volume. No-op when not running.
    async fn teardown(&self, id: Uuid) {
        let Some(running) = self.inner.running.lock().unwrap().remove(&id) else { return };
        if let Err(e) = running.vm.stop().await {
            tracing::warn!(machine = %id, error = format!("{e:#}"), "machine stop failed");
        }
        if let Some(backend) = self.inner.backends.get(running.spec.engine) {
            crate::vm::remove_with_retry(backend.as_ref(), &running.spec.name, id).await;
        }
    }

    /// Stop a machine's VM, keeping its volume, then take the snapshot the
    /// stop came with. Ignored for a boot newer than the one the stop was
    /// issued against.
    pub async fn stop(&self, id: Uuid, generation: u64, snapshot: Option<SnapshotOrder>) {
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        match self.get(id) {
            Some(r) if r.spec.generation > generation => {
                tracing::debug!(machine = %id, generation, "stale stop ignored");
                if let Some(order) = &snapshot {
                    self.inner.snapshots.refuse(order, "a newer boot of the machine is running");
                }
                return;
            }
            Some(r) => {
                let spec = r.spec.clone();
                self.teardown(id).await;
                let _ = std::fs::remove_file(self.dir(id).join("spec.json"));
                self.report(&spec, MachineState::Stopped, None, None, false);
            }
            None => {
                // Not running here: confirm, so the control plane's
                // `stopping` does not wait for a VM that does not exist.
                let _ = self.inner.up_tx.send(Up::MachineState {
                    machine_id: id,
                    generation,
                    state: MachineState::Stopped,
                    error: None,
                    volume_existed: false,
                    reason: None,
                });
            }
        }
        if let Some(order) = snapshot {
            // Registered before the lock is released: a start queued behind
            // this stop sees the capture and waits for the root disk to be
            // read -- not for the whole upload.
            let cap = self.inner.snapshots.begin(id, true);
            let snapshots = self.inner.snapshots.clone();
            tokio::spawn(async move { snapshots.capture(order, cap, None).await });
        }
    }

    /// Capture a machine's disks now: live while its VM runs here, clean
    /// when it is stopped.
    pub fn snapshot(&self, order: SnapshotOrder) {
        let running = self.get(order.machine_id);
        let cap = self.inner.snapshots.begin(order.machine_id, running.is_none());
        let snapshots = self.inner.snapshots.clone();
        tokio::spawn(async move { snapshots.capture(order, cap, running).await });
    }

    /// Drop this worker's copy of a machine that a restore moved elsewhere --
    /// unless this worker runs a boot at least that new, which would make
    /// this copy the live one.
    pub async fn reap(&self, id: Uuid, below_generation: u64) {
        if self.get(id).is_some_and(|r| r.spec.generation >= below_generation) {
            tracing::warn!(machine = %id, "refusing to reap a machine this worker runs a current boot of");
            return;
        }
        tracing::info!(machine = %id, "dropping the copy a restore replaced");
        self.destroy(id, None).await;
    }

    pub fn snapshots(&self) -> &Snapshots {
        &self.inner.snapshots
    }

    /// Stop the VM and delete the volume -- after a last snapshot, when one
    /// was ordered, and after any capture that is already reading it.
    pub async fn destroy(&self, id: Uuid, final_snapshot: Option<SnapshotOrder>) {
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        self.teardown(id).await;
        self.inner.snapshots.wait_idle(id).await;
        if let Some(order) = final_snapshot {
            let cap = self.inner.snapshots.begin(id, true);
            self.inner.snapshots.capture(order, cap, None).await;
        }
        // A VM this worker lost track of can still be registered with an
        // engine under the machine's name.
        let name = puku_cloud_proto::machine::machine_name_for(id);
        for backend in self.inner.backends.all() {
            let _ = backend.remove(&name).await;
        }
        let dir = self.dir(id);
        if dir.exists() {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => tracing::info!(machine = %id, "machine destroyed"),
                Err(e) => tracing::warn!(machine = %id, error = %e, "removing the machine volume failed"),
            }
        }
        self.inner.locks.lock().unwrap().remove(&id);
    }

    /// Reattach to machines whose VMs survived a workerd restart. Called
    /// before registering, so the Register frame lists them as running.
    pub async fn reconcile_from_disk(&self) {
        for id in self.on_disk_ids() {
            let spec_path = self.dir(id).join("spec.json");
            let Ok(bytes) = std::fs::read(&spec_path) else { continue };
            let spec: MachineSpec = match serde_json::from_slice(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(machine = %id, error = %e, "bad machine spec.json");
                    continue;
                }
            };
            let Some(backend) = self.inner.backends.get(spec.engine) else {
                let _ = std::fs::remove_file(&spec_path);
                continue;
            };
            match backend.attach(&spec.name).await {
                Ok(vm) => {
                    tracing::info!(machine = %id, "reattached to a running machine");
                    self.inner.running.lock().unwrap().insert(id, Arc::new(Running { spec, vm }));
                }
                Err(e) => {
                    // The VM is gone; the volume is not. Dropping the marker
                    // makes Register leave it out, and the control plane
                    // marks the machine stopped.
                    tracing::info!(machine = %id, error = format!("{e:#}"), "machine VM did not survive the restart");
                    let _ = std::fs::remove_file(&spec_path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::VmBackend;
    use puku_cloud_proto::machine::{Entrypoint, VolumeSpec};
    use puku_cloud_proto::Engine;

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!("puku-machines-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spec(id: Uuid, generation: u64) -> MachineSpec {
        MachineSpec {
            machine_id: id,
            name: puku_cloud_proto::machine::machine_name_for(id),
            engine: Engine::Libkrun,
            image: "alpine".into(),
            cpus: 1,
            memory_mib: 4096,
            expose: vec![6080],
            env: [("A".to_string(), "b".to_string())].into(),
            entrypoint: Some(Entrypoint { argv: vec!["/bin/serve".into(), "a b".into()], user: None }),
            volume: Some(VolumeSpec { path: "/home/user".into(), uid: 1000 }),
            max_duration_s: 0,
            generation,
            persist_root: false,
            restore: None,
        }
    }

    fn setup() -> (Machines, Arc<crate::vm::fake::FakeBackend>, mpsc::UnboundedReceiver<Up>, PathBuf) {
        let dir = scratch();
        let fake = Arc::new(crate::vm::fake::FakeBackend::new(Engine::Libkrun));
        let backends = Backends::default().with(fake.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        (Machines::new(&dir, backends, tx, vec![], false, Default::default()), fake, rx, dir)
    }

    fn states(rx: &mut mpsc::UnboundedReceiver<Up>) -> Vec<(MachineState, bool)> {
        let mut out = Vec::new();
        while let Ok(Up::MachineState { state, volume_existed, .. }) = rx.try_recv() {
            out.push((state, volume_existed));
        }
        out
    }

    #[tokio::test]
    async fn a_boot_mounts_the_volume_publishes_ports_and_starts_the_entrypoint() {
        let (m, fake, mut rx, dir) = setup();
        let id = Uuid::new_v4();
        m.assign(spec(id, 1)).await;
        assert_eq!(states(&mut rx), vec![(MachineState::Booting, false), (MachineState::Running, false)]);

        let created = fake.created().pop().unwrap();
        assert_eq!(created.ports, vec![6080]);
        assert_eq!(created.mounts[0].guest, "/home/user");
        assert!(created.mounts[0].host.starts_with(&dir));
        assert!(created.env.contains(&("A".into(), "b".into())));

        let execs = fake.execs();
        let (_, chown) = &execs[0];
        assert_eq!(
            (chown.cmd.as_str(), chown.args.as_slice(), chown.user.as_deref()),
            ("chown", &["1000:1000".to_string(), "/home/user".to_string()][..], Some("root")),
            "the volume is handed to its owner inside the guest before anything runs"
        );
        let (_, ep) = execs.last().unwrap();
        assert_eq!(ep.args[3..], ["/bin/serve".to_string(), "a b".to_string()], "argv passes through unsplit");
        assert_eq!(ep.user.as_deref(), Some("1000"), "the volume's owner by default");
        assert_eq!(m.slots(), 2, "4 GiB is two slots");
        assert_eq!(m.running_ids(), vec![id]);
    }

    /// The flag the API turns into `resumed`: false on the first boot, true
    /// once the volume is there from before.
    #[tokio::test]
    async fn the_second_boot_finds_its_volume() {
        let (m, _fake, mut rx, _dir) = setup();
        let id = Uuid::new_v4();
        m.assign(spec(id, 1)).await;
        m.stop(id, 1, None).await;
        m.assign(spec(id, 2)).await;
        let s = states(&mut rx);
        assert_eq!(s.last(), Some(&(MachineState::Running, true)), "{s:?}");
    }

    /// A stop issued against an older boot must not kill a newer one.
    #[tokio::test]
    async fn a_stale_stop_is_ignored() {
        let (m, _fake, mut rx, _dir) = setup();
        let id = Uuid::new_v4();
        m.assign(spec(id, 3)).await;
        let _ = states(&mut rx);
        m.stop(id, 2, None).await;
        assert!(m.get(id).is_some(), "still running");
        assert!(states(&mut rx).is_empty());
    }

    /// A stop for a machine not here still gets an answer, or `stopping`
    /// would wait for ever.
    #[tokio::test]
    async fn stopping_an_absent_machine_confirms_it() {
        let (m, _fake, mut rx, _dir) = setup();
        m.stop(Uuid::new_v4(), 7, None).await;
        assert_eq!(states(&mut rx), vec![(MachineState::Stopped, false)]);
    }

    #[tokio::test]
    async fn destroy_deletes_the_volume() {
        let (m, _fake, _rx, _dir) = setup();
        let id = Uuid::new_v4();
        m.assign(spec(id, 1)).await;
        assert_eq!(m.on_disk_ids(), vec![id]);
        m.destroy(id, None).await;
        assert!(m.on_disk_ids().is_empty());
        assert!(m.get(id).is_none());
    }

    /// A worker restart: the VM survived, so reattach; a VM that did not is
    /// forgotten but its volume stays.
    #[tokio::test]
    async fn reconcile_reattaches_survivors_only() {
        let (m, fake, _rx, dir) = setup();
        let alive = Uuid::new_v4();
        let dead = Uuid::new_v4();
        m.assign(spec(alive, 1)).await;
        m.assign(spec(dead, 1)).await;
        // The engine lost `dead`'s VM while workerd was down.
        let _ = fake.remove(&puku_cloud_proto::machine::machine_name_for(dead)).await;

        let (tx, _rx2) = mpsc::unbounded_channel();
        let restarted =
            Machines::new(&dir, Backends::default().with(fake.clone()), tx, vec![], false, Default::default());
        restarted.reconcile_from_disk().await;
        assert_eq!(restarted.running_ids(), vec![alive]);
        let mut on_disk = restarted.on_disk_ids();
        on_disk.sort();
        let mut want = vec![alive, dead];
        want.sort();
        assert_eq!(on_disk, want, "the dead machine's volume is kept");
    }

    #[tokio::test]
    async fn an_engine_this_worker_does_not_run_fails_the_boot() {
        let (m, _fake, mut rx, _dir) = setup();
        let mut s = spec(Uuid::new_v4(), 1);
        s.engine = Engine::CloudHypervisor;
        m.assign(s).await;
        let mut last = None;
        while let Ok(Up::MachineState { state, reason, .. }) = rx.try_recv() {
            last = Some((state, reason));
        }
        assert_eq!(
            last,
            Some((MachineState::Failed, Some("engine_unavailable".to_string()))),
            "a failed boot says why in a word controld can act on"
        );
    }

    #[test]
    fn boot_errors_carry_their_reason_through_context() {
        let refused = anyhow::Error::new(BootRefusal::new("image_not_staged", "no disk")).context("creating the VM");
        assert_eq!(crate::vm::boot_failure_reason(&refused), "image_not_staged");
        let full = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull)).context("writing");
        assert_eq!(crate::vm::boot_failure_reason(&full), "insufficient_disk");
        assert_eq!(crate::vm::boot_failure_reason(&anyhow::anyhow!("nope")), "boot_failed");
    }
}
