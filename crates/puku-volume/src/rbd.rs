//! RBD backend: session volumes as Ceph RBD images.
//!
//! Every operation is an `rbd` / `ceph` CLI call through a
//! [`CommandRunner`], and **every failure is returned** -- a fence that
//! silently "succeeds" is how split-brain happens.
//!
//! ```text
//! create    rbd clone <base-pool>/<base-image>@<snap> <session-pool>/<session-id>
//! attach    rbd device map --exclusive <pool>/<image>        -> prints /dev/rbdN
//! detach    rbd device unmap <pool>/<image>
//! snapshot  rbd snap create <pool>/<image>@puku-<unix-ms>
//! fence     rbd status <pool>/<image> --format json          -> watcher addresses
//!           ceph osd blocklist add <addr> <expire-seconds>   (one per watcher)
//! unfence   ceph osd blocklist rm <addr>
//! ```
//!
//! Fencing is per volume: the clients that currently have the image open
//! are blocklisted by their exact Ceph address. That needs no knowledge of
//! a host's IP, and it cuts off exactly the writers of that disk. A Ceph
//! blocklist expires (one hour by default), so the expiry is always passed
//! explicitly and defaults to seven days; unfencing is a deliberate act.
//!
//! `RbdBackend::for_test` keeps the old in-memory simulation for callers
//! that only need a `VolumeBackend` without a cluster.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::VolumeError;
use crate::fence::Blocklist;
use crate::runner::{CmdOutput, CommandRunner, SystemRunner};
use crate::traits::VolumeBackend;
use crate::types::{DevicePath, HostId, SnapId, VolumeId};

/// Seven days. A fenced client must never quietly regain write access.
pub const DEFAULT_BLOCKLIST_EXPIRE_S: u64 = 7 * 24 * 3600;

/// Config for the real RBD backend.
#[derive(Debug, Clone)]
pub struct RbdBackendConfig {
    /// Pool holding base images, e.g. `puku-base`.
    pub pool_base: String,
    /// Pool holding per-session clones, e.g. `puku-sessions`.
    pub pool_sessions: String,
    /// Base image used when `create` is given an empty base.
    pub base_image: String,
    /// Protected snapshot of the base image to clone from.
    pub base_snap: String,
    /// Cephx user without the `client.` prefix.
    pub ceph_user: String,
    /// Path to ceph.conf; `None` uses the tools' default.
    pub ceph_config: Option<String>,
    pub rbd_bin: String,
    pub ceph_bin: String,
    /// Seconds a fence blocklist entry lives.
    pub blocklist_expire_s: u64,
    /// Extra `-o` options for `rbd device map` (comma-joined). Tests use
    /// `noshare` so two mappings on one machine are two Ceph clients, the
    /// way two hosts would be.
    pub map_options: Vec<String>,
}

impl RbdBackendConfig {
    pub fn new(pool_base: impl Into<String>, pool_sessions: impl Into<String>) -> Self {
        Self {
            pool_base: pool_base.into(),
            pool_sessions: pool_sessions.into(),
            base_image: "agent-base".into(),
            base_snap: "v1".into(),
            ceph_user: "puku".into(),
            ceph_config: None,
            rbd_bin: "rbd".into(),
            ceph_bin: "ceph".into(),
            blocklist_expire_s: DEFAULT_BLOCKLIST_EXPIRE_S,
            map_options: Vec::new(),
        }
    }

    pub fn with_map_options(mut self, opts: &[&str]) -> Self {
        self.map_options = opts.iter().map(|o| o.to_string()).collect();
        self
    }

    pub fn with_ceph_user(mut self, u: impl Into<String>) -> Self {
        self.ceph_user = u.into();
        self
    }

    pub fn with_ceph_config(mut self, p: impl Into<String>) -> Self {
        self.ceph_config = Some(p.into());
        self
    }

    pub fn with_base(mut self, image: impl Into<String>, snap: impl Into<String>) -> Self {
        self.base_image = image.into();
        self.base_snap = snap.into();
        self
    }

    pub fn with_blocklist_expire(mut self, secs: u64) -> Self {
        self.blocklist_expire_s = secs;
        self
    }
}

#[derive(Debug)]
struct SimState {
    volumes: HashMap<VolumeId, Vec<u8>>, // fake disk payload
    attached: HashMap<VolumeId, Vec<HostId>>,
    fenced: Vec<HostId>,
    snapshots: HashMap<VolumeId, Vec<SnapId>>,
}

/// RBD backend. Production shells out through a [`CommandRunner`]; tests
/// either script the runner or use the in-memory simulation.
#[derive(Clone)]
pub struct RbdBackend {
    cfg: Option<RbdBackendConfig>,
    runner: Arc<dyn CommandRunner>,
    blocklist: Arc<Blocklist>,
    sim: Arc<Mutex<Option<SimState>>>,
}

impl std::fmt::Debug for RbdBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RbdBackend")
            .field("cfg", &self.cfg)
            .field("sim", &self.sim.lock().map(|s| s.is_some()).unwrap_or(false))
            .finish()
    }
}

impl RbdBackend {
    /// Production constructor: real `rbd` / `ceph` processes.
    pub fn new(cfg: RbdBackendConfig) -> Self {
        Self::with_runner(cfg, Arc::new(SystemRunner))
    }

    /// Production code path with an injected runner (tests script it).
    pub fn with_runner(cfg: RbdBackendConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            cfg: Some(cfg),
            runner,
            blocklist: Arc::new(Blocklist::new()),
            sim: Arc::new(Mutex::new(None)),
        }
    }

    /// In-memory simulation; never calls out to Ceph.
    pub fn for_test() -> Self {
        Self {
            cfg: None,
            runner: Arc::new(SystemRunner),
            blocklist: Arc::new(Blocklist::new()),
            sim: Arc::new(Mutex::new(Some(SimState {
                volumes: HashMap::new(),
                attached: HashMap::new(),
                fenced: Vec::new(),
                snapshots: HashMap::new(),
            }))),
        }
    }

    /// Blocklist accessor so the controld cron / runbook can audit it.
    pub fn blocklist(&self) -> &Blocklist {
        &self.blocklist
    }

    fn cfg(&self) -> Result<&RbdBackendConfig, VolumeError> {
        self.cfg.as_ref().ok_or_else(|| VolumeError::BackendUnavailable("simulation only".into()))
    }

    fn is_sim(&self) -> bool {
        self.sim.lock().expect("sim poisoned").is_some()
    }

    /// `--id <user> [--conf <path>]`, shared by `rbd` and `ceph`.
    fn auth_args(cfg: &RbdBackendConfig) -> Vec<String> {
        let mut a = vec!["--id".to_string(), cfg.ceph_user.clone()];
        if let Some(conf) = &cfg.ceph_config {
            a.push("--conf".into());
            a.push(conf.clone());
        }
        a
    }

    async fn rbd(&self, args: &[&str]) -> Result<CmdOutput, VolumeError> {
        let cfg = self.cfg()?;
        let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        all.extend(Self::auth_args(cfg));
        self.runner.run(&cfg.rbd_bin, &all).await
    }

    async fn ceph(&self, args: &[&str]) -> Result<CmdOutput, VolumeError> {
        let cfg = self.cfg()?;
        let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        all.extend(Self::auth_args(cfg));
        self.runner.run(&cfg.ceph_bin, &all).await
    }

    fn rejected(what: &str, out: &CmdOutput) -> VolumeError {
        VolumeError::Rejected(format!("{what} failed (exit {}): {}", out.status, out.stderr.trim()))
    }

    fn check_host(&self, host: HostId) -> Result<(), VolumeError> {
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_ref() {
            if sim.fenced.contains(&host) {
                return Err(VolumeError::HostFenced(host));
            }
        }
        if self.blocklist.is_blocked(host) {
            return Err(VolumeError::HostFenced(host));
        }
        Ok(())
    }

    /// The image spec for a session volume.
    pub fn session_volume(&self, session_id: Uuid) -> Result<VolumeId, VolumeError> {
        Ok(VolumeId(format!("{}/{}", self.cfg()?.pool_sessions, session_id)))
    }

    /// Ceph addresses of the clients that currently have `vol` open.
    pub async fn watchers(&self, vol: &VolumeId) -> Result<Vec<String>, VolumeError> {
        let out = self.rbd(&["status", vol.as_str(), "--format", "json"]).await?;
        if !out.success() {
            if out.stderr.contains("No such file") {
                return Err(VolumeError::NotFound(vol.to_string()));
            }
            return Err(Self::rejected("rbd status", &out));
        }
        parse_watchers(&out.stdout)
    }

    /// Blocklist every client that has `vol` open, then confirm each entry
    /// is in the cluster's blocklist. Returns the fenced addresses (empty
    /// when nobody had the volume open, which is already safe).
    pub async fn fence_volume_watchers(&self, vol: &VolumeId) -> Result<Vec<String>, VolumeError> {
        let expire = self.cfg()?.blocklist_expire_s.to_string();
        let addrs = self.watchers(vol).await?;
        for addr in &addrs {
            let out = self.ceph(&["osd", "blocklist", "add", addr, &expire]).await?;
            if !out.success() {
                return Err(VolumeError::Fence(format!(
                    "blocklisting {addr} for {vol}: {}",
                    out.stderr.trim()
                )));
            }
        }
        if !addrs.is_empty() {
            let listed = self.blocklisted().await?;
            for addr in &addrs {
                if !listed.iter().any(|l| l == addr) {
                    return Err(VolumeError::Fence(format!("{addr} missing from the blocklist after adding it")));
                }
            }
        }
        Ok(addrs)
    }

    /// Current cluster blocklist (addresses only).
    pub async fn blocklisted(&self) -> Result<Vec<String>, VolumeError> {
        let out = self.ceph(&["osd", "blocklist", "ls", "--format", "json"]).await?;
        if !out.success() {
            return Err(Self::rejected("ceph osd blocklist ls", &out));
        }
        parse_blocklist(&out.stdout)
    }

    /// Lift a fence for specific client addresses (post-recovery, explicit).
    pub async fn unfence_addrs(&self, addrs: &[String]) -> Result<(), VolumeError> {
        for addr in addrs {
            let out = self.ceph(&["osd", "blocklist", "rm", addr]).await?;
            if !out.success() && !out.stderr.contains("isn't blocklisted") {
                return Err(Self::rejected("ceph osd blocklist rm", &out));
            }
        }
        Ok(())
    }

    /// Device a volume is mapped to on this host, if any.
    /// Create an empty image for a session (no base to clone from). The
    /// filesystem is laid down by whoever mounts it first. Idempotent.
    pub async fn create_blank(&self, session_id: Uuid, size_mib: u64) -> Result<VolumeId, VolumeError> {
        let vol = self.session_volume(session_id)?;
        self.create_image(&vol, size_mib).await?;
        Ok(vol)
    }

    /// The pool session and machine images live in.
    pub fn sessions_pool(&self) -> Result<&str, VolumeError> {
        Ok(&self.cfg()?.pool_sessions)
    }

    /// An image in the sessions pool by name (machines use `machine-<id>`).
    pub fn image(&self, name: &str) -> Result<VolumeId, VolumeError> {
        Ok(VolumeId(format!("{}/{}", self.cfg()?.pool_sessions, name)))
    }

    /// Create an empty image of `size_mib`. Idempotent.
    pub async fn create_image(&self, vol: &VolumeId, size_mib: u64) -> Result<(), VolumeError> {
        let size = format!("{size_mib}M");
        let out = self.rbd(&["create", "--size", &size, vol.as_str()]).await?;
        if !out.success() && !out.stderr.contains("File exists") {
            return Err(Self::rejected("rbd create", &out));
        }
        Ok(())
    }

    /// Every image in the sessions pool, by name.
    pub async fn list_images(&self) -> Result<Vec<String>, VolumeError> {
        let pool = self.cfg()?.pool_sessions.clone();
        let out = self.rbd(&["ls", "--format", "json", &pool]).await?;
        if !out.success() {
            return Err(Self::rejected("rbd ls", &out));
        }
        parse_image_list(&out.stdout)
    }

    /// `(image spec, device)` for every RBD image mapped on this host.
    pub async fn mapped(&self) -> Result<Vec<(String, String)>, VolumeError> {
        let out = self.rbd(&["device", "list", "--format", "json"]).await?;
        if !out.success() {
            return Err(Self::rejected("rbd device list", &out));
        }
        Ok(parse_device_list(&out.stdout))
    }

    /// Unmap a device even if its client is dead (blocklisted by a fence).
    pub async fn force_unmap(&self, device: &str) -> Result<(), VolumeError> {
        let out = self.rbd(&["device", "unmap", "-o", "force", device]).await?;
        if !out.success() && !out.stderr.contains("not mapped") && !out.stderr.contains("No such") {
            return Err(Self::rejected("rbd device unmap -o force", &out));
        }
        Ok(())
    }

    /// Delete an image for good. Idempotent: an image already gone is fine.
    /// Refused by Ceph while any client still has it open.
    pub async fn remove(&self, vol: &VolumeId) -> Result<(), VolumeError> {
        let out = self.rbd(&["rm", "--no-progress", vol.as_str()]).await?;
        if !out.success() && !out.stderr.contains("No such file") {
            return Err(Self::rejected("rbd rm", &out));
        }
        Ok(())
    }

    async fn mapped_device(&self, vol: &VolumeId) -> Result<Option<String>, VolumeError> {
        let out = self.rbd(&["device", "list", "--format", "json"]).await?;
        if !out.success() {
            return Err(Self::rejected("rbd device list", &out));
        }
        Ok(parse_device_list(&out.stdout)
            .into_iter()
            .find(|(spec, _)| spec == vol.as_str())
            .map(|(_, dev)| dev))
    }
}

/// The first JSON value in `text`. The Ceph tools sometimes print a status
/// line after the JSON (`ceph osd blocklist ls` adds "listed N entries").
fn first_json(text: &str, what: &str) -> Result<serde_json::Value, VolumeError> {
    serde_json::Deserializer::from_str(text.trim())
        .into_iter::<serde_json::Value>()
        .next()
        .unwrap_or(Ok(serde_json::Value::Null))
        .map_err(|e| VolumeError::Rejected(format!("{what} json: {e}")))
}

/// `rbd ls --format json` → image names.
pub fn parse_image_list(json: &str) -> Result<Vec<String>, VolumeError> {
    let v = first_json(json, "rbd ls")?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// `rbd status --format json` → watcher addresses. Tolerates both a bare
/// list and the `{"watchers":[{"address":..}]}` object.
pub fn parse_watchers(json: &str) -> Result<Vec<String>, VolumeError> {
    let v = first_json(json, "rbd status")?;
    let list = v.get("watchers").cloned().unwrap_or(v);
    Ok(list
        .as_array()
        .map(|a| a.iter().filter_map(|w| w.get("address")?.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// `ceph osd blocklist ls --format json` → addresses.
pub fn parse_blocklist(json: &str) -> Result<Vec<String>, VolumeError> {
    let v = first_json(json, "blocklist")?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(|e| e.get("addr")?.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// `rbd device list --format json` → `(pool/image, /dev/rbdN)` pairs.
pub fn parse_device_list(json: &str) -> Vec<(String, String)> {
    let v = first_json(json, "rbd device list").unwrap_or(serde_json::Value::Null);
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let pool = d.get("pool")?.as_str()?;
                    let ns = d.get("namespace").and_then(|n| n.as_str()).unwrap_or("");
                    let image = d.get("name")?.as_str()?;
                    let dev = d.get("device")?.as_str()?.to_string();
                    let spec = if ns.is_empty() { format!("{pool}/{image}") } else { format!("{pool}/{ns}/{image}") };
                    Some((spec, dev))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl VolumeBackend for RbdBackend {
    async fn create(&self, session_id: Uuid, base: &SnapId, on_host: HostId) -> Result<VolumeId, VolumeError> {
        self.check_host(on_host)?;
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            let vol = VolumeId(format!("rbd-sessions/{session_id}"));
            sim.volumes.insert(vol.clone(), Vec::new());
            return Ok(vol);
        }
        let cfg = self.cfg()?;
        let parent = if base.volume.as_str().is_empty() {
            format!("{}/{}@{}", cfg.pool_base, cfg.base_image, cfg.base_snap)
        } else {
            format!("{}@{}", base.volume, base.name)
        };
        let vol = self.session_volume(session_id)?;
        let out = self.rbd(&["clone", &parent, vol.as_str()]).await?;
        if !out.success() && !out.stderr.contains("File exists") {
            return Err(Self::rejected("rbd clone", &out));
        }
        Ok(vol)
    }

    async fn attach(&self, vol: &VolumeId, host: HostId) -> Result<DevicePath, VolumeError> {
        self.check_host(host)?;
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            if !sim.volumes.contains_key(vol) {
                return Err(VolumeError::NotFound(vol.to_string()));
            }
            sim.attached.entry(vol.clone()).or_default().push(host);
            return Ok(DevicePath(format!("/dev/rbd/sim/{}", vol.as_str().replace('/', "_")).into()));
        }
        if let Some(dev) = self.mapped_device(vol).await? {
            return Ok(DevicePath(dev.into()));
        }
        // --exclusive: hold the exclusive lock and never hand it over, so a
        // second host cannot map this disk read-write behind our back.
        let opts = self.cfg()?.map_options.join(",");
        let mut args = vec!["device", "map", "--exclusive"];
        if !opts.is_empty() {
            args.extend(["-o", opts.as_str()]);
        }
        args.push(vol.as_str());
        let out = self.rbd(&args).await?;
        if !out.success() {
            if out.stderr.contains("No such file") {
                return Err(VolumeError::NotFound(vol.to_string()));
            }
            return Err(Self::rejected("rbd device map", &out));
        }
        let dev = out.stdout.trim();
        if !dev.starts_with("/dev/") {
            return Err(VolumeError::Rejected(format!("rbd device map printed {dev:?}, not a device")));
        }
        Ok(DevicePath(dev.into()))
    }

    async fn detach(&self, vol: &VolumeId, host: HostId) -> Result<(), VolumeError> {
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            if let Some(hosts) = sim.attached.get_mut(vol) {
                hosts.retain(|h| *h != host);
            }
            return Ok(());
        }
        if self.mapped_device(vol).await?.is_none() {
            return Ok(()); // idempotent
        }
        let out = self.rbd(&["device", "unmap", vol.as_str()]).await?;
        if !out.success() {
            return Err(Self::rejected("rbd device unmap", &out));
        }
        Ok(())
    }

    async fn snapshot(&self, vol: &VolumeId, on_host: HostId) -> Result<SnapId, VolumeError> {
        self.check_host(on_host)?;
        let name = format!("puku-{}", chrono::Utc::now().timestamp_millis());
        let snap = SnapId::new(vol.clone(), name.clone());
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            sim.snapshots.entry(vol.clone()).or_default().push(snap.clone());
            return Ok(snap);
        }
        let out = self.rbd(&["snap", "create", &format!("{vol}@{name}")]).await?;
        if !out.success() {
            return Err(VolumeError::Snapshot(format!("rbd snap create: {}", out.stderr.trim())));
        }
        Ok(snap)
    }

    async fn relocate(&self, vol: &VolumeId, from: HostId, to: HostId) -> Result<DevicePath, VolumeError> {
        self.detach(vol, from).await?;
        self.attach(vol, to).await
    }

    /// Host-level fence: refuses every later operation for `host` through
    /// this backend. The Ceph-level cut-off is [`fence_volume`], which the
    /// recovery path calls for each volume it moves.
    async fn fence(&self, host: HostId) -> Result<(), VolumeError> {
        self.blocklist.block(host)?;
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            if !sim.fenced.contains(&host) {
                sim.fenced.push(host);
            }
        }
        Ok(())
    }

    async fn unfence(&self, host: HostId) -> Result<(), VolumeError> {
        self.blocklist.unblock(host)?;
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            sim.fenced.retain(|h| *h != host);
        }
        Ok(())
    }

    async fn fence_volume(&self, vol: &VolumeId) -> Result<Vec<String>, VolumeError> {
        if self.is_sim() {
            return Ok(Vec::new());
        }
        self.fence_volume_watchers(vol).await
    }

    async fn is_reachable(&self, vol: &VolumeId, host: HostId) -> Result<bool, VolumeError> {
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_ref() {
            return Ok(sim.attached.get(vol).map(|hosts| hosts.contains(&host)).unwrap_or(false));
        }
        // Only this host's own mappings are observable from here.
        Ok(self.mapped_device(vol).await?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;
    use crate::traits::VolumeBackend;

    fn backend(answers: Vec<CmdOutput>) -> (RbdBackend, Arc<ScriptedRunner>) {
        let runner = Arc::new(ScriptedRunner::new(answers));
        let cfg = RbdBackendConfig::new("puku-base", "puku-sessions").with_ceph_config("/etc/ceph/ceph.conf");
        (RbdBackend::with_runner(cfg, runner.clone()), runner)
    }

    fn host() -> HostId {
        HostId(Uuid::new_v4())
    }

    #[tokio::test]
    async fn create_clones_from_configured_base() {
        let (b, r) = backend(vec![CmdOutput::ok("")]);
        let sid = Uuid::new_v4();
        let vol = b.create(sid, &SnapId::new(VolumeId(String::new()), ""), host()).await.unwrap();
        assert_eq!(vol.as_str(), format!("puku-sessions/{sid}"));
        assert_eq!(
            r.calls(),
            vec![format!("rbd clone puku-base/agent-base@v1 puku-sessions/{sid} --id puku --conf /etc/ceph/ceph.conf")]
        );
    }

    #[tokio::test]
    async fn create_is_idempotent_but_other_failures_surface() {
        let (b, _) = backend(vec![CmdOutput::fail(17, "rbd: clone error: (17) File exists")]);
        assert!(b.create(Uuid::new_v4(), &SnapId::new(VolumeId(String::new()), ""), host()).await.is_ok());
        let (b, _) = backend(vec![CmdOutput::fail(2, "rbd: error opening pool: (2) No such file or directory")]);
        assert!(matches!(
            b.create(Uuid::new_v4(), &SnapId::new(VolumeId(String::new()), ""), host()).await,
            Err(VolumeError::Rejected(_))
        ));
    }

    #[test]
    fn image_list_parses_the_real_format() {
        assert_eq!(parse_image_list(r#"["abc","machine-x"]"#).unwrap(), vec!["abc", "machine-x"]);
        assert!(parse_image_list("[]").unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_blank_sizes_the_image_and_is_idempotent() {
        let (b, r) = backend(vec![CmdOutput::ok(""), CmdOutput::fail(17, "rbd: create error: (17) File exists")]);
        let sid = Uuid::new_v4();
        assert_eq!(b.create_blank(sid, 2048).await.unwrap().as_str(), format!("puku-sessions/{sid}"));
        assert!(b.create_blank(sid, 2048).await.is_ok());
        assert!(r.calls()[0].starts_with(&format!("rbd create --size 2048M puku-sessions/{sid}")));
    }

    #[tokio::test]
    async fn remove_tolerates_a_missing_image_but_not_an_open_one() {
        let (b, _) = backend(vec![CmdOutput::fail(2, "rbd: delete error: (2) No such file or directory")]);
        assert!(b.remove(&VolumeId("puku-sessions/gone".into())).await.is_ok());
        let (b, _) = backend(vec![CmdOutput::fail(16, "rbd: error: image still has watchers")]);
        assert!(b.remove(&VolumeId("puku-sessions/open".into())).await.is_err());
    }

    #[tokio::test]
    async fn attach_maps_exclusively_and_uses_printed_device() {
        let (b, r) = backend(vec![CmdOutput::ok("[]"), CmdOutput::ok("/dev/rbd3\n")]);
        let vol = VolumeId("puku-sessions/abc".into());
        let dev = b.attach(&vol, host()).await.unwrap();
        assert_eq!(dev.as_path(), std::path::Path::new("/dev/rbd3"));
        assert!(r.calls()[1].starts_with("rbd device map --exclusive puku-sessions/abc"));
    }

    #[tokio::test]
    async fn attach_reuses_existing_mapping() {
        let list = r#"[{"id":"0","pool":"puku-sessions","namespace":"","name":"abc","snap":"-","device":"/dev/rbd0"}]"#;
        let (b, r) = backend(vec![CmdOutput::ok(list)]);
        let dev = b.attach(&VolumeId("puku-sessions/abc".into()), host()).await.unwrap();
        assert_eq!(dev.as_path(), std::path::Path::new("/dev/rbd0"));
        assert_eq!(r.calls().len(), 1, "no second map when already mapped");
    }

    #[tokio::test]
    async fn attach_failure_is_an_error() {
        let (b, _) = backend(vec![CmdOutput::ok("[]"), CmdOutput::fail(16, "rbd: map failed: (16) Device or resource busy")]);
        assert!(b.attach(&VolumeId("puku-sessions/abc".into()), host()).await.is_err());
    }

    #[tokio::test]
    async fn fence_blocklists_every_watcher_with_explicit_expiry_and_verifies() {
        let status = r#"{"watchers":[{"address":"10.0.0.5:0/1111","client":4235,"cookie":1}]}"#;
        let ls = r#"[{"addr":"10.0.0.5:0/1111","until":"2026-10-12T00:00:00"}]"#;
        let (b, r) = backend(vec![CmdOutput::ok(status), CmdOutput::ok(""), CmdOutput::ok(ls)]);
        let fenced = b.fence_volume(&VolumeId("puku-sessions/abc".into())).await.unwrap();
        assert_eq!(fenced, vec!["10.0.0.5:0/1111".to_string()]);
        let calls = r.calls();
        assert!(calls[1].starts_with(&format!("ceph osd blocklist add 10.0.0.5:0/1111 {}", DEFAULT_BLOCKLIST_EXPIRE_S)));
        assert!(calls[2].starts_with("ceph osd blocklist ls --format json"));
    }

    #[tokio::test]
    async fn fence_fails_loudly_when_blocklist_add_fails() {
        let status = r#"{"watchers":[{"address":"10.0.0.5:0/1111"}]}"#;
        let (b, _) = backend(vec![CmdOutput::ok(status), CmdOutput::fail(13, "Error EACCES: access denied")]);
        assert!(matches!(b.fence_volume(&VolumeId("puku-sessions/abc".into())).await, Err(VolumeError::Fence(_))));
    }

    #[tokio::test]
    async fn fence_fails_when_entry_does_not_show_up() {
        let status = r#"{"watchers":[{"address":"10.0.0.5:0/1111"}]}"#;
        let (b, _) = backend(vec![CmdOutput::ok(status), CmdOutput::ok(""), CmdOutput::ok("[]")]);
        assert!(matches!(b.fence_volume(&VolumeId("puku-sessions/abc".into())).await, Err(VolumeError::Fence(_))));
    }

    #[tokio::test]
    async fn fence_with_no_watchers_is_a_safe_no_op() {
        let (b, r) = backend(vec![CmdOutput::ok(r#"{"watchers":[]}"#)]);
        assert!(b.fence_volume(&VolumeId("puku-sessions/abc".into())).await.unwrap().is_empty());
        assert_eq!(r.calls().len(), 1);
    }

    #[tokio::test]
    async fn snapshot_failure_is_an_error() {
        let (b, _) = backend(vec![CmdOutput::fail(1, "boom")]);
        assert!(matches!(b.snapshot(&VolumeId("puku-sessions/abc".into()), host()).await, Err(VolumeError::Snapshot(_))));
    }

    #[test]
    fn parsers_tolerate_shapes() {
        assert_eq!(parse_watchers(r#"[{"address":"a"}]"#).unwrap(), vec!["a"]);
        assert!(parse_watchers(r#"{"watchers":[]}"#).unwrap().is_empty());
        assert_eq!(parse_blocklist(r#"[{"addr":"x","until":"t"}]"#).unwrap(), vec!["x"]);
        // Real `ceph osd blocklist ls` output: JSON, then a status line.
        assert_eq!(parse_blocklist("[{\"addr\":\"x\",\"until\":\"t\"}]
listed 1 entries
").unwrap(), vec!["x"]);
        assert!(parse_device_list("not json").is_empty());
    }

    #[tokio::test]
    async fn sim_create_attach_write() {
        let b = RbdBackend::for_test();
        let host = host();
        let base = SnapId::new(VolumeId("rbd-base/seed".into()), "snap");
        let vol = b.create(Uuid::new_v4(), &base, host).await.unwrap();
        let dev = b.attach(&vol, host).await.unwrap();
        assert!(dev.as_path().starts_with("/dev/rbd/sim"));
        assert!(b.is_reachable(&vol, host).await.unwrap());
    }

    #[tokio::test]
    async fn sim_fence_blocks_attach() {
        let b = RbdBackend::for_test();
        let host = host();
        let vol = b.create(Uuid::new_v4(), &SnapId::new(VolumeId("rbd-base/seed".into()), "snap"), host).await.unwrap();
        b.attach(&vol, host).await.unwrap();
        b.fence(host).await.unwrap();
        assert!(matches!(b.attach(&vol, host).await, Err(VolumeError::HostFenced(_))));
        b.unfence(host).await.unwrap();
        b.attach(&vol, host).await.unwrap();
    }
}
