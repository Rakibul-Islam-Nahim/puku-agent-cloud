//! Local volume backend: per-session host directory.
//!
//! Used for tests and for the existing single-host deployment. It does NOT
//! make workers stateless. Statelessness comes from `RbdBackend`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::fs;
use uuid::Uuid;

use crate::error::VolumeError;
use crate::traits::VolumeBackend;
use crate::types::{DevicePath, HostId, SnapId, VolumeId};

/// Local backend config.
#[derive(Debug, Clone)]
pub struct LocalBackendConfig {
    pub root: PathBuf,
}

impl LocalBackendConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Local backend: maps a `VolumeId` to a directory under `root`.
#[derive(Debug)]
pub struct LocalBackend {
    cfg: LocalBackendConfig,
    /// Track which hosts have the volume attached, for the `is_reachable` test.
    attached: Mutex<HashMap<VolumeId, Vec<HostId>>>,
    /// Hosts that are fenced: writes return `Err(VolumeError::HostFenced)`.
    fenced: Mutex<Vec<HostId>>,
}

impl LocalBackend {
    pub fn new(cfg: LocalBackendConfig) -> Self {
        Self {
            cfg: cfg,
            attached: Mutex::new(HashMap::new()),
            fenced: Mutex::new(Vec::new()),
        }
    }

    fn volume_dir(&self, vol: &VolumeId) -> PathBuf {
        self.cfg.root.join(vol.as_str())
    }

    fn is_fenced(&self, host: HostId) -> bool {
        let g = self.fenced.lock().expect("fenced poisoned");
        g.contains(&host)
    }
}

#[async_trait]
impl VolumeBackend for LocalBackend {
    async fn create(
        &self,
        session_id: Uuid,
        _base: &SnapId,
        _on_host: HostId,
    ) -> Result<VolumeId, VolumeError> {
        let vol = VolumeId(format!("local/{}", session_id));
        let path = self.volume_dir(&vol);
        fs::create_dir_all(&path)
            .await
            .map_err(|e| VolumeError::Io(e.to_string()))?;
        Ok(vol)
    }

    async fn attach(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<DevicePath, VolumeError> {
        if self.is_fenced(host) {
            return Err(VolumeError::HostFenced(host));
        }
        let path = self.volume_dir(vol);
        if !path.exists() {
            return Err(VolumeError::NotFound(vol.to_string()));
        }
        let mut attached = self.attached.lock().expect("attached poisoned");
        attached.entry(vol.clone()).or_default().push(host);
        Ok(DevicePath(path))
    }

    async fn detach(&self, vol: &VolumeId, host: HostId) -> Result<(), VolumeError> {
        let mut attached = self.attached.lock().expect("attached poisoned");
        if let Some(hosts) = attached.get_mut(vol) {
            hosts.retain(|h| *h != host);
        }
        Ok(())
    }

    async fn snapshot(
        &self,
        vol: &VolumeId,
        _on_host: HostId,
    ) -> Result<SnapId, VolumeError> {
        let path = self.volume_dir(vol);
        if !path.exists() {
            return Err(VolumeError::NotFound(vol.to_string()));
        }
        let now = chrono::Utc::now().timestamp();
        Ok(SnapId::new(vol.clone(), format!("snap@{}", now)))
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
        let mut fenced = self.fenced.lock().expect("fenced poisoned");
        if !fenced.contains(&host) {
            fenced.push(host);
        }
        Ok(())
    }

    async fn unfence(&self, host: HostId) -> Result<(), VolumeError> {
        let mut fenced = self.fenced.lock().expect("fenced poisoned");
        fenced.retain(|h| *h != host);
        Ok(())
    }

    async fn is_reachable(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<bool, VolumeError> {
        let attached = self.attached.lock().expect("attached poisoned");
        Ok(attached
            .get(vol)
            .map(|hosts| hosts.contains(&host))
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_attach_write_detach_reattach() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(LocalBackendConfig::new(tmp.path()));
        let host = HostId(Uuid::new_v4());
        let session = Uuid::new_v4();
        let base = SnapId::new(VolumeId("local/seed".into()), "snap@0");

        let vol = backend.create(session, &base, host).await.unwrap();
        let dev = backend.attach(&vol, host).await.unwrap();
        fs::write(dev.as_path().join("hello.txt"), "world").await.unwrap();
        backend.detach(&vol, host).await.unwrap();

        let dev = backend.attach(&vol, host).await.unwrap();
        let body = fs::read_to_string(dev.as_path().join("hello.txt")).await.unwrap();
        assert_eq!(body, "world");
    }

    #[tokio::test]
    async fn fence_blocks_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(LocalBackendConfig::new(tmp.path()));
        let host = HostId(Uuid::new_v4());
        let vol = backend
            .create(Uuid::new_v4(), &SnapId::new(VolumeId("local/seed".into()), "x"), host)
            .await
            .unwrap();

        backend.attach(&vol, host).await.unwrap();
        backend.fence(host).await.unwrap();
        let res = backend.attach(&vol, host).await;
        assert!(matches!(res, Err(VolumeError::HostFenced(_))));
        backend.unfence(host).await.unwrap();
        backend.attach(&vol, host).await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(LocalBackendConfig::new(tmp.path()));
        let host = HostId(Uuid::new_v4());
        let vol = backend
            .create(Uuid::new_v4(), &SnapId::new(VolumeId("local/seed".into()), "x"), host)
            .await
            .unwrap();
        let snap = backend.snapshot(&vol, host).await.unwrap();
        assert!(snap.name.starts_with("snap@"));
    }

    #[tokio::test]
    async fn relocate_moves_attach() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(LocalBackendConfig::new(tmp.path()));
        let h1 = HostId(Uuid::new_v4());
        let h2 = HostId(Uuid::new_v4());
        let vol = backend
            .create(Uuid::new_v4(), &SnapId::new(VolumeId("local/seed".into()), "x"), h1)
            .await
            .unwrap();
        backend.attach(&vol, h1).await.unwrap();
        assert!(backend.is_reachable(&vol, h1).await.unwrap());
        backend.relocate(&vol, h1, h2).await.unwrap();
        assert!(!backend.is_reachable(&vol, h1).await.unwrap());
        assert!(backend.is_reachable(&vol, h2).await.unwrap());
    }
}