//! Where a session's files live on this worker.
//!
//! `Local` is how it always worked: `<state>/sessions/<id>/{session,workspace}`
//! on this host's disk, so the session can only ever run here again.
//!
//! `Rbd` puts them on a Ceph RBD image per session, mounted at
//! `<state>/sessions/<id>/disk` while the session runs here and unmounted
//! and unmapped when it stops. Any worker on the same cluster can then open
//! it next -- which is what lets a session outlive its host. The image is
//! mapped `--exclusive`, so two hosts can never have it open read-write at
//! once; moving it off a host that died is controld's job, and controld
//! fences that host first.
//!
//! What stays on this host's own disk in both modes is the small
//! `<state>/sessions/<id>/spec.json`, which records that this worker is
//! running the session (the restart-reconcile index).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use puku_volume::{CommandRunner, HostId, RbdBackend, SystemRunner, VolumeBackend, VolumeId};
use uuid::Uuid;

pub fn session_base_dir(state_dir: &Path, session_id: Uuid) -> PathBuf {
    state_dir.join("sessions").join(session_id.to_string())
}

#[derive(Clone)]
pub enum SessionVolumes {
    Local,
    Rbd(Arc<RbdVolumes>),
}

pub struct RbdVolumes {
    pub backend: RbdBackend,
    /// Size of a new session image. Thin-provisioned: only what is written
    /// takes space.
    pub size_mib: u64,
    /// Runs `mountpoint`, `blkid`, `mkfs.ext4`, `mount`, `umount`.
    pub runner: Arc<dyn CommandRunner>,
}

impl RbdVolumes {
    pub fn new(backend: RbdBackend, size_mib: u64) -> Self {
        Self { backend, size_mib, runner: Arc::new(SystemRunner) }
    }
}

/// `HostId` is only consulted by the backend's in-memory simulation.
const THIS_HOST: HostId = HostId(Uuid::nil());

impl SessionVolumes {
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Rbd(_))
    }

    /// Where `session/` and `workspace/` live. Valid once `open` returned.
    pub fn data_dir(&self, state_dir: &Path, session_id: Uuid) -> PathBuf {
        let base = session_base_dir(state_dir, session_id);
        match self {
            Self::Local => base,
            Self::Rbd(_) => base.join("disk"),
        }
    }

    /// Make the session's files available here, creating them on first use.
    /// Idempotent: a session already open here (a workerd restart under a
    /// running VM) is left as it is.
    pub async fn open(&self, state_dir: &Path, session_id: Uuid) -> Result<PathBuf> {
        let dir = self.data_dir(state_dir, session_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let Self::Rbd(r) = self else { return Ok(dir) };
        let vol = r.backend.session_volume(session_id)?;
        r.open_image(&vol, &dir, r.size_mib, "the session's disk").await?;
        Ok(dir)
    }

    /// Release the session's files from this host so another can open them:
    /// unmount (which flushes) and unmap. Idempotent.
    pub async fn close(&self, state_dir: &Path, session_id: Uuid) -> Result<()> {
        let Self::Rbd(r) = self else { return Ok(()) };
        let vol = r.backend.session_volume(session_id)?;
        r.close_image(&vol, &self.data_dir(state_dir, session_id)).await
    }

    /// Delete the session's files for good.
    pub async fn destroy(&self, state_dir: &Path, session_id: Uuid) -> Result<()> {
        let base = session_base_dir(state_dir, session_id);
        if let Self::Rbd(r) = self {
            self.close(state_dir, session_id).await?;
            let vol: VolumeId = r.backend.session_volume(session_id)?;
            r.backend.remove(&vol).await.with_context(|| format!("deleting {vol}"))?;
        }
        remove_dir(&base)
    }
}

/// Whether `dir` is a mountpoint: a different device from its parent.
/// Synchronous, for the restart reconcile.
pub fn is_mountpoint(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (Ok(me), Some(parent)) = (std::fs::metadata(dir), dir.parent()) else { return false };
        std::fs::metadata(parent).is_ok_and(|p| p.dev() != me.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

/// Where `device` is mounted, from `/proc/mounts`.
fn mountpoints_of(device: &str) -> Vec<String> {
    std::fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            (f.next() == Some(device)).then(|| f.next().map(str::to_string)).flatten()
        })
        .collect()
}

#[derive(Debug, Default, PartialEq)]
pub struct StaleCleanup {
    /// Images this host still had mapped but no longer runs.
    pub released: Vec<String>,
    /// Local session directories dropped (their disk lives in Ceph).
    pub dirs_removed: Vec<Uuid>,
}

impl RbdVolumes {
    /// Cleanup when this host starts (or comes back after being declared
    /// dead): every image of our pool still mapped here that nothing here
    /// runs is unmounted and force-unmapped. After a fence its client is
    /// blocklisted and every write fails anyway; dropping the mapping is
    /// what lets this host map disks again without a reboot, and frees
    /// what it was holding. `keep` names the images still in use here.
    pub async fn release_stale(&self, keep: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
        let pool = self.backend.sessions_pool()?.to_string();
        let mut released = Vec::new();
        for (spec, dev) in self.backend.mapped().await? {
            let Some(name) = spec.strip_prefix(&format!("{pool}/")) else { continue };
            if keep(name) {
                continue;
            }
            for mnt in mountpoints_of(&dev) {
                // Lazy: a dead client's mount can hang a plain umount.
                if let Err(e) = self.run_ok("umount", &["-l", &mnt]).await {
                    tracing::warn!(%spec, %mnt, error = format!("{e:#}"), "unmounting a stale disk failed");
                }
            }
            self.backend.force_unmap(&dev).await.with_context(|| format!("unmapping stale {spec}"))?;
            tracing::warn!(%spec, %dev, "released a disk this host no longer runs");
            released.push(spec);
        }
        Ok(released)
    }
}

fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

/// A machine's state directory, `<state>/machines/<id>`: its volume, its
/// kept root disk and the spec marker. On RBD the whole directory is the
/// mounted image `machine-<id>`, so the volume *and* the root disk (the
/// packages installed into it) move with the machine. Paths inside it are
/// the same either way.
#[derive(Clone)]
pub enum MachineDisks {
    Local,
    Rbd { volumes: Arc<RbdVolumes>, size_mib: u64 },
}

impl MachineDisks {
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Rbd { .. })
    }

    pub fn image_name(machine_id: Uuid) -> String {
        format!("machine-{machine_id}")
    }

    /// Make `dir` hold the machine's disk, creating it on first use.
    /// Idempotent.
    pub async fn open(&self, dir: &Path, machine_id: Uuid) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let Self::Rbd { volumes, size_mib } = self else { return Ok(()) };
        let vol = volumes.backend.image(&Self::image_name(machine_id))?;
        volumes.open_image(&vol, dir, *size_mib, "the machine's disk").await
    }

    /// Release the disk from this host. Idempotent.
    pub async fn close(&self, dir: &Path, machine_id: Uuid) -> Result<()> {
        let Self::Rbd { volumes, .. } = self else { return Ok(()) };
        let vol = volumes.backend.image(&Self::image_name(machine_id))?;
        volumes.close_image(&vol, dir).await
    }

    /// Drop this host's state for the machine. The RBD image itself is
    /// deleted only when `delete_image`: a copy-cleanup on a host the
    /// machine moved away from must never delete the one disk it moved with.
    pub async fn remove(&self, dir: &Path, machine_id: Uuid, delete_image: bool) -> Result<()> {
        if let Self::Rbd { volumes, .. } = self {
            self.close(dir, machine_id).await?;
            if delete_image {
                let vol = volumes.backend.image(&Self::image_name(machine_id))?;
                volumes.backend.remove(&vol).await.with_context(|| format!("deleting {vol}"))?;
            }
        }
        remove_dir(dir)
    }
}

impl RbdVolumes {
    /// Create (first time), map `--exclusive`, format (first time) and mount
    /// `vol` at `dir`. A no-op when `dir` is already its mountpoint.
    async fn open_image(&self, vol: &VolumeId, dir: &Path, size_mib: u64, what: &str) -> Result<()> {
        if self.is_mounted(dir).await? {
            return Ok(());
        }
        self.backend.create_image(vol, size_mib).await.with_context(|| format!("creating {what}"))?;
        let dev = self.backend.attach(vol, THIS_HOST).await.map_err(|e| {
            anyhow::anyhow!("{what} {vol} could not be opened here ({e}); another host may still hold it")
        })?;
        let dev = dev.as_path().to_string_lossy().to_string();
        if !self.has_filesystem(&dev).await? {
            self.run_ok("mkfs.ext4", &["-q", "-F", "-L", "puku", &dev]).await?;
        }
        self.run_ok("mount", &["-o", "noatime", &dev, &dir.to_string_lossy()]).await?;
        tracing::info!(volume = %vol, device = %dev, path = %dir.display(), "disk mounted");
        Ok(())
    }

    /// Unmount (which flushes) and unmap. Never unmaps a disk it could not
    /// unmount. Idempotent.
    async fn close_image(&self, vol: &VolumeId, dir: &Path) -> Result<()> {
        if self.is_mounted(dir).await? {
            self.run_ok("umount", &[&dir.to_string_lossy()]).await?;
        }
        self.backend.detach(vol, THIS_HOST).await.with_context(|| format!("unmapping {vol}"))?;
        tracing::info!(volume = %vol, "disk released");
        Ok(())
    }

    async fn is_mounted(&self, dir: &Path) -> Result<bool> {
        let out = self.runner.run("mountpoint", &["-q".to_string(), dir.to_string_lossy().to_string()]).await?;
        Ok(out.success())
    }

    /// `blkid -p` exits 2 when it finds nothing on the device.
    async fn has_filesystem(&self, dev: &str) -> Result<bool> {
        let args = ["-p", "-s", "TYPE", "-o", "value", dev].map(String::from);
        let out = self.runner.run("blkid", &args).await?;
        match out.status {
            0 => Ok(true),
            2 => Ok(false),
            n => bail!("blkid {dev} failed (exit {n}): {}", out.stderr.trim()),
        }
    }

    async fn run_ok(&self, program: &str, args: &[&str]) -> Result<()> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let out = self.runner.run(program, &args).await?;
        if !out.success() {
            bail!("{program} {} failed (exit {}): {}", args.join(" "), out.status, out.stderr.trim());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puku_volume::{CmdOutput, RbdBackendConfig, ScriptedRunner};

    /// One scripted runner behind both the backend and the mount helpers, so
    /// the test sees every command in order.
    fn rbd(answers: Vec<CmdOutput>) -> (SessionVolumes, Arc<ScriptedRunner>) {
        let runner = Arc::new(ScriptedRunner::new(answers));
        let cfg = RbdBackendConfig::new("puku-base", "puku-sessions");
        let backend = RbdBackend::with_runner(cfg, runner.clone());
        let v = RbdVolumes { backend, size_mib: 1024, runner: runner.clone() };
        (SessionVolumes::Rbd(Arc::new(v)), runner)
    }

    fn programs(r: &ScriptedRunner) -> Vec<String> {
        r.calls().iter().map(|c| c.split(' ').take(2).collect::<Vec<_>>().join(" ")).collect()
    }

    #[tokio::test]
    async fn first_open_creates_maps_formats_and_mounts() {
        let tmp = std::env::temp_dir().join(format!("puku-vol-{}", Uuid::new_v4()));
        let sid = Uuid::new_v4();
        let (v, r) = rbd(vec![
            CmdOutput::fail(1, ""),           // mountpoint: not mounted
            CmdOutput::ok(""),                // rbd create
            CmdOutput::ok("[]"),              // rbd device list
            CmdOutput::ok("/dev/rbd7\n"),     // rbd device map
            CmdOutput::fail(2, ""),           // blkid: nothing there
            CmdOutput::ok(""),                // mkfs.ext4
            CmdOutput::ok(""),                // mount
        ]);
        let dir = v.open(&tmp, sid).await.unwrap();
        assert_eq!(dir, session_base_dir(&tmp, sid).join("disk"));
        assert_eq!(
            programs(&r),
            ["mountpoint -q", "rbd create", "rbd device", "rbd device", "blkid -p", "mkfs.ext4 -q", "mount -o"]
        );
        assert!(r.calls()[3].starts_with("rbd device map --exclusive"));
        assert!(r.calls()[6].contains("/dev/rbd7"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn reopening_an_existing_disk_never_reformats_it() {
        let tmp = std::env::temp_dir().join(format!("puku-vol-{}", Uuid::new_v4()));
        let (v, r) = rbd(vec![
            CmdOutput::fail(1, ""),
            CmdOutput::fail(17, "rbd: create error: (17) File exists"),
            CmdOutput::ok("[]"),
            CmdOutput::ok("/dev/rbd2\n"),
            CmdOutput::ok("ext4\n"),          // blkid: already has a filesystem
            CmdOutput::ok(""),                // mount
        ]);
        v.open(&tmp, Uuid::new_v4()).await.unwrap();
        assert!(!programs(&r).iter().any(|p| p.starts_with("mkfs")), "{:?}", r.calls());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn an_open_session_is_left_alone() {
        let tmp = std::env::temp_dir().join(format!("puku-vol-{}", Uuid::new_v4()));
        let (v, r) = rbd(vec![CmdOutput::ok("")]); // mountpoint: mounted
        v.open(&tmp, Uuid::new_v4()).await.unwrap();
        assert_eq!(r.calls().len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn a_disk_held_elsewhere_fails_with_a_reason() {
        let tmp = std::env::temp_dir().join(format!("puku-vol-{}", Uuid::new_v4()));
        let (v, _) = rbd(vec![
            CmdOutput::fail(1, ""),
            CmdOutput::ok(""),
            CmdOutput::ok("[]"),
            CmdOutput::fail(16, "rbd: map failed: (16) Device or resource busy"),
        ]);
        let err = format!("{:#}", v.open(&tmp, Uuid::new_v4()).await.unwrap_err());
        assert!(err.contains("another host may still hold it"), "{err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn close_unmounts_then_unmaps() {
        let tmp = std::env::temp_dir();
        let sid = Uuid::new_v4();
        let list = format!(r#"[{{"pool":"puku-sessions","namespace":"","name":"{sid}","device":"/dev/rbd7"}}]"#);
        let (v, r) = rbd(vec![
            CmdOutput::ok(""),      // mountpoint: mounted
            CmdOutput::ok(""),      // umount
            CmdOutput::ok(&list),   // rbd device list
            CmdOutput::ok(""),      // rbd device unmap
        ]);
        v.close(&tmp, sid).await.unwrap();
        assert_eq!(programs(&r)[0], "mountpoint -q");
        assert!(r.calls()[1].starts_with("umount ") && r.calls()[1].ends_with("/disk"), "{:?}", r.calls());
        assert!(r.calls()[2].starts_with("rbd device list"));
        assert!(r.calls()[3].starts_with(&format!("rbd device unmap puku-sessions/{sid}")));
    }

    #[tokio::test]
    async fn a_failed_unmount_is_not_followed_by_an_unmap() {
        let (v, r) = rbd(vec![CmdOutput::ok(""), CmdOutput::fail(32, "umount: target is busy")]);
        assert!(v.close(&std::env::temp_dir(), Uuid::new_v4()).await.is_err());
        assert_eq!(r.calls().len(), 2, "never unmap a disk still mounted");
    }

    #[tokio::test]
    async fn local_volumes_never_run_a_command() {
        let tmp = std::env::temp_dir().join(format!("puku-vol-{}", Uuid::new_v4()));
        let sid = Uuid::new_v4();
        let dir = SessionVolumes::Local.open(&tmp, sid).await.unwrap();
        assert_eq!(dir, session_base_dir(&tmp, sid));
        SessionVolumes::Local.close(&tmp, sid).await.unwrap();
        SessionVolumes::Local.destroy(&tmp, sid).await.unwrap();
        assert!(!dir.exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn startup_cleanup_releases_only_what_nothing_here_runs() {
        let list = r#"[{"pool":"p","namespace":"","name":"keep-me","device":"/dev/rbd1"},
                       {"pool":"p","namespace":"","name":"stale","device":"/dev/rbd2"},
                       {"pool":"other","namespace":"","name":"not-ours","device":"/dev/rbd3"}]"#;
        let runner = Arc::new(ScriptedRunner::new(vec![CmdOutput::ok(list), CmdOutput::ok("")]));
        let backend = RbdBackend::with_runner(RbdBackendConfig::new("p", "p"), runner.clone());
        let v = RbdVolumes { backend, size_mib: 1, runner: runner.clone() };
        let released = v.release_stale(&|name| name == "keep-me").await.unwrap();
        assert_eq!(released, vec!["p/stale"]);
        let calls = runner.calls();
        assert!(calls[1].starts_with("rbd device unmap -o force /dev/rbd2"), "{calls:?}");
        assert_eq!(calls.len(), 2, "nothing else touched: {calls:?}");
    }

    // --- Against a real Ceph cluster (PUKU_TEST_CEPH=1, root, cephx user
    // `puku`, pool `puku-sessions`). Two `noshare` backends on one machine
    // act as two hosts.

    fn real_enabled() -> bool {
        std::env::var("PUKU_TEST_CEPH").as_deref() == Ok("1")
    }

    fn real_host() -> SessionVolumes {
        let cfg = RbdBackendConfig::new("puku-sessions", "puku-sessions").with_map_options(&["noshare"]);
        SessionVolumes::Rbd(Arc::new(RbdVolumes::new(RbdBackend::new(cfg), 256)))
    }

    fn sh(cmd: &str) -> (i32, String) {
        let out = std::process::Command::new("sh").arg("-c").arg(cmd).output().expect("sh");
        (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    fn state_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("puku-real-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn real_ceph_a_session_disk_follows_the_session_between_hosts() {
        if !real_enabled() {
            return;
        }
        let (a, b) = (real_host(), real_host());
        let (sa, sb) = (state_dir("a"), state_dir("b"));
        let sid = Uuid::new_v4();

        let dir = a.open(&sa, sid).await.expect("open on A");
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        std::fs::write(dir.join("workspace/notes.txt"), "written on A").unwrap();
        a.close(&sa, sid).await.expect("close on A");
        assert_ne!(sh(&format!("mountpoint -q {}", dir.display())).0, 0, "unmounted on A");

        let dir = b.open(&sb, sid).await.expect("open on B");
        assert_eq!(std::fs::read_to_string(dir.join("workspace/notes.txt")).unwrap(), "written on A");
        b.close(&sb, sid).await.expect("close on B");

        b.destroy(&sb, sid).await.expect("destroy");
        let (rc, _) = sh(&format!("rbd info puku-sessions/{sid} --id puku"));
        assert_ne!(rc, 0, "the image is gone");
        std::fs::remove_dir_all(&sa).ok();
    }

    /// A dies holding the disk. B is refused until A is fenced; then B
    /// opens it and finds everything A had synced.
    #[tokio::test]
    async fn real_ceph_a_crashed_host_must_be_fenced_before_another_opens_the_disk() {
        if !real_enabled() {
            return;
        }
        let (a, b) = (real_host(), real_host());
        let (sa, sb) = (state_dir("a"), state_dir("b"));
        let sid = Uuid::new_v4();
        let vol = VolumeId(format!("puku-sessions/{sid}"));

        let dir_a = a.open(&sa, sid).await.expect("open on A");
        std::fs::write(dir_a.join("fsynced.txt"), "made it to disk").unwrap();
        assert_eq!(sh(&format!("sync -f {}", dir_a.display())).0, 0);
        // A crashes here: no close.

        // B is refused while A holds the exclusive lock. A raw map: on one
        // machine B's backend would reuse A's mapping from the shared
        // kernel, which a separate host cannot see.
        let (rc, out) = sh(&format!("rbd device map --exclusive -o noshare {vol} --id puku"));
        assert_ne!(rc, 0, "B must be refused while A holds the disk: {out}");

        let SessionVolumes::Rbd(rb) = &b else { unreachable!() };
        let fenced = rb.backend.fence_volume(&vol).await.expect("fence A");
        assert!(!fenced.is_empty(), "A's client is cut off");

        // A's machine is gone: on one box, drop its mount and mapping the
        // way a dead host's simply vanish.
        let (_, list) = sh("rbd device list --format json --id puku");
        let dev = puku_volume::rbd::parse_device_list(&list)
            .into_iter()
            .find(|(spec, _)| spec == vol.as_str())
            .map(|(_, d)| d)
            .expect("A's mapping");
        assert_eq!(sh(&format!("umount -l {}", dir_a.display())).0, 0);
        sh(&format!("rbd device unmap -o force {dev}"));

        let dir_b = b.open(&sb, sid).await.expect("B opens the disk after the fence");
        assert_eq!(std::fs::read_to_string(dir_b.join("fsynced.txt")).unwrap(), "made it to disk");

        b.destroy(&sb, sid).await.expect("destroy");
        rb.backend.unfence_addrs(&fenced).await.expect("unfence");
        std::fs::remove_dir_all(&sa).ok();
    }

    #[tokio::test]
    async fn a_machine_cleanup_never_deletes_the_shared_image() {
        let runner = Arc::new(ScriptedRunner::new(vec![
            CmdOutput::fail(32, ""),   // mountpoint: not mounted
            CmdOutput::ok("[]"),       // rbd device list: not mapped
        ]));
        let backend = RbdBackend::with_runner(RbdBackendConfig::new("p", "p"), runner.clone());
        let disks = MachineDisks::Rbd {
            volumes: Arc::new(RbdVolumes { backend, size_mib: 1, runner: runner.clone() }),
            size_mib: 1,
        };
        let dir = std::env::temp_dir().join(format!("puku-m-{}", Uuid::new_v4()));
        disks.remove(&dir, Uuid::new_v4(), false).await.unwrap();
        assert!(!r_has(&runner, "rbd rm"), "{:?}", runner.calls());
    }

    fn r_has(r: &ScriptedRunner, prefix: &str) -> bool {
        r.calls().iter().any(|c| c.starts_with(prefix))
    }

    /// A machine's volume and its kept root disk move together.
    #[tokio::test]
    async fn real_ceph_a_machine_disk_moves_with_its_volume_and_root_disk() {
        if !real_enabled() {
            return;
        }
        let host = || {
            let cfg = RbdBackendConfig::new("puku-sessions", "puku-sessions").with_map_options(&["noshare"]);
            MachineDisks::Rbd { volumes: Arc::new(RbdVolumes::new(RbdBackend::new(cfg), 256)), size_mib: 256 }
        };
        let (a, b) = (host(), host());
        let id = Uuid::new_v4();
        let (da, db) = (state_dir("ma").join(id.to_string()), state_dir("mb").join(id.to_string()));

        a.open(&da, id).await.expect("open on A");
        std::fs::create_dir_all(da.join("volume")).unwrap();
        std::fs::write(da.join("volume/notes.txt"), "user files").unwrap();
        std::fs::write(da.join("root.image"), "pukubot-computer:latest").unwrap();
        a.close(&da, id).await.expect("close on A");

        b.open(&db, id).await.expect("open on B");
        assert_eq!(std::fs::read_to_string(db.join("volume/notes.txt")).unwrap(), "user files");
        assert_eq!(std::fs::read_to_string(db.join("root.image")).unwrap(), "pukubot-computer:latest");
        b.close(&db, id).await.unwrap();

        // A's cleanup of its old state leaves the image alone...
        a.remove(&da, id, false).await.unwrap();
        assert_eq!(sh(&format!("rbd info puku-sessions/machine-{id} --id puku")).0, 0, "image kept");
        // ...an explicit destroy deletes it.
        b.remove(&db, id, true).await.unwrap();
        assert_ne!(sh(&format!("rbd info puku-sessions/machine-{id} --id puku")).0, 0, "image deleted");
    }

    /// A host that died holding a disk comes back: its startup cleanup
    /// drops the dead mapping, and the disk opens again.
    #[tokio::test]
    async fn real_ceph_startup_cleanup_releases_a_disk_left_mapped_by_a_crash() {
        if !real_enabled() {
            return;
        }
        let cfg = || RbdBackendConfig::new("puku-sessions", "puku-sessions").with_map_options(&["noshare"]);
        let v = real_host();
        let sd = state_dir("crash");
        let sid = Uuid::new_v4();
        let dir = v.open(&sd, sid).await.expect("open");
        std::fs::write(dir.join("kept.txt"), "still here").unwrap();
        assert_eq!(sh(&format!("sync -f {}", dir.display())).0, 0);
        // Crash: no close. The restart's cleanup runs with nothing kept.
        let rbd = RbdVolumes::new(RbdBackend::new(cfg()), 256);
        let released = rbd.release_stale(&|_| false).await.expect("cleanup");
        assert!(released.contains(&format!("puku-sessions/{sid}")), "{released:?}");
        assert!(!is_mountpoint(&dir), "unmounted");
        let (_, list) = sh("rbd device list --format json --id puku");
        assert!(!list.contains(&sid.to_string()), "unmapped: {list}");
        // The disk opens again, data intact, and is listed in the pool.
        let dir = v.open(&sd, sid).await.expect("reopen");
        assert_eq!(std::fs::read_to_string(dir.join("kept.txt")).unwrap(), "still here");
        assert!(rbd.backend.list_images().await.unwrap().contains(&sid.to_string()));
        v.destroy(&sd, sid).await.unwrap();
        assert!(!rbd.backend.list_images().await.unwrap().contains(&sid.to_string()));
    }
}
