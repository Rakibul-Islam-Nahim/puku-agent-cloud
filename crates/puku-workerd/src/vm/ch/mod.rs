//! The Cloud Hypervisor engine.
//!
//! One VM is: a read-only root disk built from the OCI image, a sparse
//! per-VM disk the guest overlays on top of it, one virtiofsd per shared
//! host directory, a TAP on a /30 of its own, and a cloud-hypervisor process
//! booting a pinned kernel straight into `puku-guestd`. Everything the host
//! keeps for it lives in `<vms_dir>/<name>/`, and `vm.json` there is what a
//! restarted worker reattaches from.
//!
//! What msb does that this does not: network-boundary secret injection
//! (refused at create; it is off in production anyway).

pub mod agent;
pub mod dns;
pub mod image;
pub mod launch;
pub mod net;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use puku_cloud_proto::guest_proto::{GuestReply, GuestRequest};
use puku_cloud_proto::Engine;
use serde::{Deserialize, Serialize};

use super::{ExecOutput, ExecRequest, GuestIo, StreamOutcome, Vm, VmBackend, VmSpec};
use launch::Launch;
use net::NetSlot;

/// Where this worker's Cloud Hypervisor toolchain and state live.
#[derive(Debug, Clone)]
pub struct ChConfig {
    pub ch_bin: PathBuf,
    pub virtiofsd_bin: PathBuf,
    /// An uncompressed kernel: `vmlinux` (PVH) on x86_64, `Image` on aarch64.
    pub kernel: PathBuf,
    pub images_dir: PathBuf,
    pub vms_dir: PathBuf,
    /// Size of each VM's writable disk. Sparse: only what the guest writes
    /// takes space.
    pub upper_gib: u64,
    /// Default size of the guest's /dev/shm, for VMs that name none.
    pub shm: String,
    /// Give memory the guest frees back to the host (see `Launch`).
    pub free_page_reporting: bool,
}

/// What `vm.json` records: enough to find, reattach to and tear down a VM
/// after this process is gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VmRecord {
    name: String,
    slot: u32,
    #[serde(default)]
    egress_allow: Vec<String>,
    /// Under systemd, the unit; otherwise the launcher's pid, which leads
    /// its own process group.
    systemd: bool,
    #[serde(default)]
    pid: Option<u32>,
}

struct Inner {
    cfg: ChConfig,
    systemd: bool,
    version: String,
    slots: Mutex<HashSet<u32>>,
    /// Each VM's resolver runs in this process; a restarted worker starts
    /// them again on attach.
    resolvers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

pub struct ChBackend {
    inner: Arc<Inner>,
}

/// A sparse ext4 disk of `gib`: only what the guest writes takes space.
async fn format_disk(path: &Path, gib: u64) -> Result<()> {
    std::fs::File::create(path)?.set_len(gib << 30)?;
    let mkfs = tokio::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-E", "lazy_itable_init=1,lazy_journal_init=1"])
        .arg(path)
        .output()
        .await
        .context("running mkfs.ext4")?;
    if !mkfs.status.success() {
        // Half a disk must not pass for a kept root on the next boot.
        let _ = std::fs::remove_file(path);
        bail!("formatting the VM disk: {}", String::from_utf8_lossy(&mkfs.stderr).trim());
    }
    Ok(())
}

fn run_ok(program: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {}", program.display()))?;
    if !out.status.success() {
        bail!("{} {}: {}", program.display(), args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl ChBackend {
    /// The engine, if this host can actually run it. Every check here is
    /// something whose absence would otherwise surface as a session failing
    /// to boot -- better to not advertise the engine at all.
    pub fn probe(cfg: ChConfig) -> Result<ChBackend> {
        if !cfg!(target_os = "linux") {
            bail!("Cloud Hypervisor runs on Linux with KVM only");
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .context("/dev/kvm is not usable (bare metal or nested virtualization required)")?;
        for dev in ["/dev/vhost-vsock", "/dev/net/tun"] {
            if !Path::new(dev).exists() {
                bail!("{dev} is missing (load the vhost_vsock and tun modules)");
            }
        }
        for (what, p) in [("cloud-hypervisor", &cfg.ch_bin), ("virtiofsd", &cfg.virtiofsd_bin), ("the guest kernel", &cfg.kernel)] {
            if !p.is_file() {
                bail!("{what} is missing at {} (deploy/scripts/prestage-ch.sh)", p.display());
            }
        }
        for tool in ["nft", "ip", "mkfs.ext4"] {
            if launch::which(tool).is_none() {
                bail!("{tool} is not on PATH");
            }
        }
        let version = run_ok(&cfg.ch_bin, &["--version"]).unwrap_or_else(|_| "unknown".into());
        net::ensure_base().context("setting up the VM network table")?;
        std::fs::create_dir_all(&cfg.vms_dir)?;
        let systemd = launch::has_systemd();
        let backend = ChBackend {
            inner: Arc::new(Inner {
                cfg,
                systemd,
                version,
                slots: Mutex::new(HashSet::new()),
                resolvers: Mutex::new(HashMap::new()),
            }),
        };
        // Slots held by VMs that outlived the last worker.
        for rec in backend.records() {
            backend.inner.slots.lock().unwrap().insert(rec.slot);
        }
        Ok(backend)
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.inner.cfg.vms_dir.join(name)
    }

    fn record(&self, name: &str) -> Option<VmRecord> {
        let bytes = std::fs::read(self.dir(name).join("vm.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn records(&self) -> Vec<VmRecord> {
        let Ok(rd) = std::fs::read_dir(&self.inner.cfg.vms_dir) else { return Vec::new() };
        rd.filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| self.record(n)))
            .collect()
    }

    fn claim_slot(&self) -> Result<u32> {
        let mut used = self.inner.slots.lock().unwrap();
        let slot = (1..=net::MAX_SLOTS).find(|s| !used.contains(s)).context("the VM address pool is exhausted")?;
        used.insert(slot);
        Ok(slot)
    }

    fn alive(&self, rec: &VmRecord) -> bool {
        if rec.systemd {
            std::process::Command::new("systemctl")
                .args(["is-active", "--quiet", &format!("puku-vm-{}", rec.name)])
                .status()
                .is_ok_and(|s| s.success())
        } else {
            rec.pid.is_some_and(|p| Path::new(&format!("/proc/{p}")).exists())
        }
    }

    fn start_resolver(&self, rec: &VmRecord) {
        let handle = dns::spawn(
            NetSlot::new(rec.slot),
            dns::Policy { allow: rec.egress_allow.clone(), upstream: dns::host_upstream() },
        );
        if let Some(old) = self.inner.resolvers.lock().unwrap().insert(rec.name.clone(), handle) {
            old.abort();
        }
    }

    /// Stop a VM's processes, hard if it does not go quietly.
    async fn kill(&self, rec: &VmRecord) {
        if rec.systemd {
            let _ = tokio::process::Command::new("systemctl")
                .args(["stop", &format!("puku-vm-{}", rec.name)])
                .status()
                .await;
            return;
        }
        if let Some(pid) = rec.pid {
            let group = format!("-{pid}");
            let _ = tokio::process::Command::new("kill").args(["-TERM", "--", &group]).status().await;
            for _ in 0..50 {
                if !self.alive(rec) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = tokio::process::Command::new("kill").args(["-KILL", "--", &group]).status().await;
        }
    }

    fn console_tail(&self, name: &str) -> String {
        let log = std::fs::read_to_string(self.dir(name).join("console.log")).unwrap_or_default();
        let lines: Vec<&str> = log.lines().rev().take(8).collect();
        lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
    }

    async fn boot(&self, spec: &VmSpec, dir: &Path, slot: u32) -> Result<()> {
        let cfg = &self.inner.cfg;
        let rootfs = image::rootfs(&cfg.images_dir, &spec.image)?;

        // A machine that keeps its root disk brings it along; everything else
        // gets a fresh one each boot, gone with the VM's directory.
        let upper = match &spec.root_disk {
            Some(disk) if disk.is_file() => disk.clone(),
            Some(disk) => {
                format_disk(disk, cfg.upper_gib).await?;
                disk.clone()
            }
            None => {
                let disk = dir.join("upper.ext4");
                format_disk(&disk, cfg.upper_gib).await?;
                disk
            }
        };

        let netslot = NetSlot::new(slot);
        net::up(&netslot, !spec.egress_allow.is_empty())?;
        let rec = VmRecord {
            name: spec.name.clone(),
            slot,
            egress_allow: spec.egress_allow.clone(),
            systemd: self.inner.systemd,
            pid: None,
        };
        self.start_resolver(&rec);

        let l = Launch {
            name: spec.name.clone(),
            dir: dir.to_path_buf(),
            ch_bin: cfg.ch_bin.clone(),
            virtiofsd_bin: cfg.virtiofsd_bin.clone(),
            kernel: cfg.kernel.clone(),
            rootfs,
            upper,
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            shares: spec
                .mounts
                .iter()
                .enumerate()
                .map(|(i, m)| (format!("fs{i}"), m.host.clone(), m.guest.clone()))
                .collect(),
            net: netslot,
            max_duration_s: spec.max_duration_s,
            shm: spec.shm_mib.map_or_else(|| cfg.shm.clone(), |mib| format!("{mib}m")),
            free_page_reporting: cfg.free_page_reporting,
        };
        std::fs::write(dir.join("launch.sh"), l.script())?;
        std::fs::write(dir.join("labels.json"), serde_json::to_vec(&spec.labels)?)?;

        let rec = if self.inner.systemd {
            let status = tokio::process::Command::new("systemd-run")
                .args(l.systemd_run_args())
                .status()
                .await
                .context("running systemd-run")?;
            if !status.success() {
                bail!("systemd-run could not start {}", l.unit());
            }
            rec
        } else {
            let log = std::fs::File::create(dir.join("launch.log"))?;
            let child = std::process::Command::new("setsid")
                .arg("/bin/sh")
                .arg(dir.join("launch.sh"))
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()
                .context("starting the VM launcher")?;
            VmRecord { pid: Some(child.id()), ..rec }
        };
        std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&rec)?)?;

        let vsock = dir.join("vsock.sock");
        agent::wait_ready(&vsock, Duration::from_secs(60))
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}; console: {}", self.console_tail(&spec.name)))?;
        let env = spec.env.iter().cloned().collect();
        match agent::call(&vsock, &GuestRequest::Init { env, hostname: spec.name.clone() }).await? {
            GuestReply::Ok => Ok(()),
            other => bail!("the guest agent refused init: {other:?}"),
        }
    }
}

#[async_trait]
impl VmBackend for ChBackend {
    fn engine(&self) -> Engine {
        Engine::CloudHypervisor
    }

    fn version(&self) -> String {
        self.inner.version.clone()
    }

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>> {
        if !spec.secrets.is_empty() {
            bail!("network-boundary secret injection is not supported on cloud_hypervisor");
        }
        let dir = self.dir(&spec.name);
        if dir.exists() {
            // Left behind by a boot that died with the box.
            self.remove(&spec.name).await?;
        }
        std::fs::create_dir_all(&dir)?;
        let slot = self.claim_slot()?;
        if let Err(e) = self.boot(spec, &dir, slot).await {
            let _ = self.remove(&spec.name).await;
            // `remove` releases the slot only when vm.json made it that far.
            self.inner.slots.lock().unwrap().remove(&slot);
            return Err(e);
        }
        Ok(Box::new(ChVm { name: spec.name.clone(), vsock: dir.join("vsock.sock"), backend: self.clone_handle() }))
    }

    async fn attach(&self, name: &str) -> Result<Box<dyn Vm>> {
        let rec = self.record(name).context("sandbox gone after restart: no vm.json")?;
        if !self.alive(&rec) {
            bail!("sandbox gone after restart: its VMM is not running");
        }
        let vsock = self.dir(name).join("vsock.sock");
        agent::wait_ready(&vsock, Duration::from_secs(10)).await.context("reattach failed")?;
        self.inner.slots.lock().unwrap().insert(rec.slot);
        self.start_resolver(&rec);
        Ok(Box::new(ChVm { name: name.to_string(), vsock, backend: self.clone_handle() }))
    }

    async fn remove(&self, name: &str) -> Result<()> {
        let dir = self.dir(name);
        if !dir.exists() {
            return Ok(());
        }
        if let Some(rec) = self.record(name) {
            self.kill(&rec).await;
            net::down(&NetSlot::new(rec.slot));
            if let Some(h) = self.inner.resolvers.lock().unwrap().remove(name) {
                h.abort();
            }
            self.inner.slots.lock().unwrap().remove(&rec.slot);
        }
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<String>> {
        Ok(self.records().into_iter().filter(|r| self.alive(r)).map(|r| r.name).collect())
    }

    async fn prepull(&self, image: &str) -> Result<()> {
        image::rootfs(&self.inner.cfg.images_dir, image).map(|_| ())
    }

    fn staged_images(&self) -> Option<Vec<puku_cloud_proto::worker_proto::StagedImage>> {
        Some(image::staged(&self.inner.cfg.images_dir))
    }
}

impl ChBackend {
    fn clone_handle(&self) -> ChBackend {
        ChBackend { inner: self.inner.clone() }
    }
}

struct ChVm {
    name: String,
    vsock: PathBuf,
    backend: ChBackend,
}

#[async_trait]
impl Vm for ChVm {
    fn name(&self) -> &str {
        &self.name
    }

    async fn exec(&self, req: ExecRequest) -> Result<ExecOutput> {
        agent::exec(&self.vsock, req).await
    }

    async fn exec_stream(
        &self,
        req: ExecRequest,
        stdin: Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
        stdout: tokio::sync::mpsc::Sender<bytes::Bytes>,
    ) -> Result<StreamOutcome> {
        agent::exec_stream(&self.vsock, req, stdin, stdout).await
    }

    async fn connect_port(&self, port: u16) -> Result<Box<dyn GuestIo>> {
        Ok(Box::new(agent::connect_port(&self.vsock, port).await?))
    }

    /// Power the guest off from inside -- it syncs its disks first -- and
    /// only then make sure the processes are gone.
    async fn stop(&self) -> Result<()> {
        let _ = tokio::time::timeout(Duration::from_secs(5), agent::call(&self.vsock, &GuestRequest::Shutdown)).await;
        if let Some(rec) = self.backend.record(&self.name) {
            for _ in 0..100 {
                if !self.backend.alive(&rec) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            self.backend.kill(&rec).await;
        }
        Ok(())
    }
}
