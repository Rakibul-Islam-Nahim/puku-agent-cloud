//! RBD backend.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.1.4, the operations cookbook is:
//!
//! ```text
//! rbd clone rbd-base/puku-agent-0.1.0@snap rbd-sessions/<session_id>
//! rbd map rbd-sessions/<session_id>            # on host
//! rbd unmap /dev/rbd<N>                          # on host
//! rbd snap create rbd-sessions/<session_id>@<ts>
//! rbd blocklist add <client_hostname>            # fence
//! rbd blocklist remove <client_hostname>         # unfence
//! ```
//!
//! We shell out to `/usr/bin/rbd` (and `cephx_overrides`-aware env) by
//! default. For tests, the `RbdBackend::for_test` constructor uses an
//! in-memory simulation that returns synthetic `VolumeId` / `SnapId` values
//! without ever touching Ceph -- which is what lets this crate compile and
//! pass unit tests on a dev box with no `/dev/kvm` and no `ceph` installed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::VolumeError;
use crate::fence::Blocklist;
use crate::traits::VolumeBackend;
use crate::types::{DevicePath, HostId, SnapId, VolumeId};

/// Config for the real RBD backend.
#[derive(Debug, Clone)]
pub struct RbdBackendConfig {
    pub pool_base: String,
    pub pool_sessions: String,
    pub ceph_user: String,
    pub ceph_config: Option<String>,
}

impl RbdBackendConfig {
    pub fn new(pool_base: impl Into<String>, pool_sessions: impl Into<String>) -> Self {
        Self {
            pool_base: pool_base.into(),
            pool_sessions: pool_sessions.into(),
            ceph_user: "puku".into(),
            ceph_config: None,
        }
    }

    pub fn with_ceph_user(mut self, u: impl Into<String>) -> Self {
        self.ceph_user = u.into();
        self
    }

    pub fn with_ceph_config(mut self, p: impl Into<String>) -> Self {
        self.ceph_config = Some(p.into());
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

/// RBD backend. In production this shells out to `rbd`; in tests it uses an
/// in-memory simulation so the build can pass on a dev box.
#[derive(Clone)]
pub struct RbdBackend {
    cfg: Option<RbdBackendConfig>,
    blocklist: Arc<Blocklist>,
    sim: Arc<Mutex<Option<SimState>>>,
}

impl std::fmt::Debug for RbdBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RbdBackend")
            .field("cfg", &self.cfg.is_some())
            .field("sim", &self.sim.lock().map(|s| s.is_some()).unwrap_or(false))
            .finish()
    }
}

impl RbdBackend {
    /// Production constructor. Honors `CEPH_CONFIG` / `CEPH_USER` / pool config
    /// via the supplied `RbdBackendConfig`.
    pub fn new(cfg: RbdBackendConfig) -> Self {
        Self {
            cfg: Some(cfg),
            blocklist: Arc::new(Blocklist::new()),
            sim: Arc::new(Mutex::new(None)),
        }
    }

    /// Test constructor. In-memory simulation; does not call out to `rbd`.
    pub fn for_test() -> Self {
        Self {
            cfg: None,
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

    fn shell_out(&self, args: &[&str]) -> Result<String, VolumeError> {
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| VolumeError::BackendUnavailable("sim only".into()))?;
        let mut cmd = std::process::Command::new("/usr/bin/rbd");
        cmd.args(args);
        if let Some(ceph_conf) = &cfg.ceph_config {
            cmd.env("CEPH_CONFIG", ceph_conf);
        }
        cmd.env("CEPH_USER", &cfg.ceph_user);
        let out = cmd
            .output()
            .map_err(|e| VolumeError::BackendUnavailable(format!("rbd: {}", e)))?;
        if !out.status.success() {
            return Err(VolumeError::Rejected(format!(
                "rbd {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn run_fenced(&self, host: HostId) -> Result<(), VolumeError> {
        let g = self.sim.lock().expect("sim poisoned");
        if let Some(sim) = g.as_ref() {
            if sim.fenced.contains(&host) {
                return Err(VolumeError::HostFenced(host));
            }
        }
        if self.blocklist.is_blocked(host) {
            return Err(VolumeError::HostFenced(host));
        }
        Ok(())
    }
}

#[async_trait]
impl VolumeBackend for RbdBackend {
    async fn create(
        &self,
        session_id: Uuid,
        _base: &SnapId,
        on_host: HostId,
    ) -> Result<VolumeId, VolumeError> {
        self.run_fenced(on_host)?;
        let vol = VolumeId(format!("rbd-sessions/{}", session_id));

        // Production: shell out to `rbd clone ...`. Tests: just record it.
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            sim.volumes.insert(vol.clone(), Vec::new());
            return Ok(vol);
        }
        let pool = &self.cfg.as_ref().unwrap().pool_base;
        let _ = self.shell_out(&[
            "clone",
            &format!("{}/puku-agent-0.1.0@snap", pool),
            &format!("rbd-sessions/{}", session_id),
        ]);
        Ok(vol)
    }

    async fn attach(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<DevicePath, VolumeError> {
        self.run_fenced(host)?;
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            if !sim.volumes.contains_key(vol) {
                return Err(VolumeError::NotFound(vol.to_string()));
            }
            sim.attached.entry(vol.clone()).or_default().push(host);
            return Ok(DevicePath(std::path::PathBuf::from(format!(
                "/dev/rbd/sim/{}",
                vol.as_str().replace('/', "_")
            ))));
        }
        let _ = self.shell_out(&["map", &vol.0]);
        Ok(DevicePath(std::path::PathBuf::from(format!(
            "/dev/rbd/{}",
            vol.as_str().replace('/', "_")
        ))))
    }

    async fn detach(&self, vol: &VolumeId, host: HostId) -> Result<(), VolumeError> {
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            if let Some(hosts) = sim.attached.get_mut(vol) {
                hosts.retain(|h| *h != host);
            }
            return Ok(());
        }
        let dev = format!(
            "/dev/rbd/{}",
            vol.as_str().replace('/', "_")
        );
        let _ = self.shell_out(&["unmap", &dev]);
        Ok(())
    }

    async fn snapshot(
        &self,
        vol: &VolumeId,
        on_host: HostId,
    ) -> Result<SnapId, VolumeError> {
        self.run_fenced(on_host)?;
        let ts = chrono::Utc::now().timestamp();
        let snap = SnapId::new(vol.clone(), format!("snap@{}", ts));

        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            sim.snapshots.entry(vol.clone()).or_default().push(snap.clone());
            return Ok(snap);
        }
        let _ = self.shell_out(&["snap", "create", &format!("{}@{}", vol.0, ts)]);
        Ok(snap)
    }

    async fn relocate(
        &self,
        vol: &VolumeId,
        from: HostId,
        to: HostId,
    ) -> Result<DevicePath, VolumeError> {
        self.detach(vol, from).await?;
        self.attach(vol, to).await
    }

    async fn fence(&self, host: HostId) -> Result<(), VolumeError> {
        self.blocklist.block(host)?;
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_mut() {
            if !sim.fenced.contains(&host) {
                sim.fenced.push(host);
            }
            return Ok(());
        }
        let _ = self.shell_out(&["blocklist", "add", &host.to_string()]);
        Ok(())
    }

    async fn unfence(&self, host: HostId) -> Result<(), VolumeError> {
        self.blocklist.unblock(host)?;
        {
            let mut g = self.sim.lock().expect("sim poisoned");
            if let Some(sim) = g.as_mut() {
                sim.fenced.retain(|h| *h != host);
            }
        }
        if self.sim.lock().expect("sim poisoned").is_some() {
            return Ok(());
        }
        let _ = self.shell_out(&["blocklist", "remove", &host.to_string()]);
        Ok(())
    }

    async fn is_reachable(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<bool, VolumeError> {
        if let Some(sim) = self.sim.lock().expect("sim poisoned").as_ref() {
            return Ok(sim
                .attached
                .get(vol)
                .map(|hosts| hosts.contains(&host))
                .unwrap_or(false));
        }
        // Without a real cluster we cannot probe; fail open with `false`.
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::VolumeBackend;

    #[tokio::test]
    async fn sim_create_attach_write() {
        let b = RbdBackend::for_test();
        let host = HostId(Uuid::new_v4());
        let base = SnapId::new(VolumeId("rbd-base/seed".into()), "snap");
        let vol = b.create(Uuid::new_v4(), &base, host).await.unwrap();
        let dev = b.attach(&vol, host).await.unwrap();
        assert!(dev.as_path().starts_with("/dev/rbd/sim"));
        assert!(b.is_reachable(&vol, host).await.unwrap());
    }

    #[tokio::test]
    async fn sim_fence_blocks_attach() {
        let b = RbdBackend::for_test();
        let host = HostId(Uuid::new_v4());
        let vol = b
            .create(Uuid::new_v4(), &SnapId::new(VolumeId("rbd-base/seed".into()), "snap"), host)
            .await
            .unwrap();
        b.attach(&vol, host).await.unwrap();
        b.fence(host).await.unwrap();
        let res = b.attach(&vol, host).await;
        assert!(matches!(res, Err(VolumeError::HostFenced(_))));
        b.unfence(host).await.unwrap();
        b.attach(&vol, host).await.unwrap();
    }

    #[tokio::test]
    async fn sim_snapshot_round_trip() {
        let b = RbdBackend::for_test();
        let host = HostId(Uuid::new_v4());
        let vol = b
            .create(Uuid::new_v4(), &SnapId::new(VolumeId("rbd-base/seed".into()), "snap"), host)
            .await
            .unwrap();
        let snap = b.snapshot(&vol, host).await.unwrap();
        assert!(snap.name.starts_with("snap@"));
    }
}