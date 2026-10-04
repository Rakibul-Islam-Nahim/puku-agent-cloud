//! Restore ladder (RSD §1.1 + §4.4.1).
//!
//! Every recovery uses the same ladder, in this order. Earlier rows preserve
//! more; later rows are the floor.

use async_trait::async_trait;
use uuid::Uuid;

use crate::manifest::{Manifest, ManifestStore, SnapshotStatus};
use puku_volume::VolumeId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Disk + memory from a Durable manifest. RPO = age of that manifest.
    Warm(Manifest),
    /// Memory missing or engine-incompatible. Disk is the **current RBD
    /// head** -- no rollback, no package loss. Agent re-reads transcript;
    /// in-flight tools re-run.
    ColdHead(VolumeId),
    /// Disk volume lost. Restored disk from the off-cluster copy (R2 disk
    /// archive for archived sessions, otherwise the latest disk backup chain).
    /// Memory not available; full RAM loss.
    FromArchive(Manifest),
    /// Disk lost AND archive lost. Re-created disk from base image, replayed
    /// `installed_packages`, replayed event log from `last_seq`. Keeps
    /// environment and conversation; loses data files.
    Rebuilt(VolumeId),
    /// All exhausted. Loud failure.
    Failed(String),
}

impl RestoreOutcome {
    pub fn kind(&self) -> &'static str {
        match self {
            RestoreOutcome::Warm(_) => "Warm",
            RestoreOutcome::ColdHead(_) => "ColdHead",
            RestoreOutcome::FromArchive(_) => "FromArchive",
            RestoreOutcome::Rebuilt(_) => "Rebuilt",
            RestoreOutcome::Failed(_) => "Failed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("manifest {0} not restorable: status={1:?}")]
    NotRestorable(Uuid, SnapshotStatus),
    #[error("no manifests for session {0}")]
    NoManifests(Uuid),
}

/// Pluggable backend for picking a manifest and applying it.
#[async_trait]
pub trait RestoreBackend: Send + Sync {
    /// Choose the manifest we should restore from.
    /// `prefer_local` is true when the host holds the bytes (LocalDurable
    /// is acceptable).
    async fn pick_manifest(
        &self,
        session_id: Uuid,
        prefer_local: bool,
    ) -> Result<Option<Manifest>, RestoreError>;

    /// Current RBD head volume for the session.
    async fn current_volume(&self, session_id: Uuid) -> Result<Option<VolumeId>, RestoreError>;
}

/// Default restore service. Walks the ladder:
/// 1. Warm (latest Durable, or LocalDurable when prefer_local)
/// 2. ColdHead (current RBD head, no rollback)
/// 3. FromArchive (last manifest with verified R2 copy)
/// 4. Rebuilt (current volume if any -- a fresh base would be the next step)
/// 5. Failed
pub struct RestoreService<B: RestoreBackend> {
    pub backend: B,
}

impl<B: RestoreBackend> RestoreService<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub async fn restore(
        &self,
        session_id: Uuid,
        prefer_local: bool,
    ) -> Result<RestoreOutcome, RestoreError> {
        match self.backend.pick_manifest(session_id, prefer_local).await? {
            Some(m) => {
                if prefer_local && m.status.acceptable_for_local_restore() {
                    return Ok(RestoreOutcome::Warm(m));
                }
                if !prefer_local && m.status.acceptable_for_remote_restore() {
                    return Ok(RestoreOutcome::Warm(m));
                }
                if m.r2_sha256.is_some() {
                    return Ok(RestoreOutcome::FromArchive(m));
                }
                // fall through
            }
            None => {}
        }
        match self.backend.current_volume(session_id).await? {
            Some(v) => Ok(RestoreOutcome::ColdHead(v)),
            None => Err(RestoreError::NoManifests(session_id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use crate::manifest::SnapshotStatus;

    struct StubBackend {
        m: Mutex<Option<Manifest>>,
        vol: Mutex<Option<VolumeId>>,
    }
    impl StubBackend {
        fn new(m: Option<Manifest>, vol: Option<VolumeId>) -> Self {
            Self {
                m: Mutex::new(m),
                vol: Mutex::new(vol),
            }
        }
    }
    #[async_trait]
    impl RestoreBackend for StubBackend {
        async fn pick_manifest(
            &self,
            _session_id: Uuid,
            _prefer_local: bool,
        ) -> Result<Option<Manifest>, RestoreError> {
            Ok(self.m.lock().await.clone())
        }
        async fn current_volume(
            &self,
            _session_id: Uuid,
        ) -> Result<Option<VolumeId>, RestoreError> {
            Ok(self.vol.lock().await.clone())
        }
    }

    #[tokio::test]
    async fn restores_warm_for_durable() {
        let mut m = Manifest::new_pending(
            Uuid::new_v4(),
            "rbd-sessions/s@1",
            "mem/k",
            100,
            true,
            None,
            Uuid::new_v4(),
            vec![],
            "kvm",
            "h",
        );
        m.status = SnapshotStatus::Durable;
        let backend = StubBackend::new(Some(m), Some(VolumeId("rbd-sessions/x".into())));
        let svc = RestoreService::new(backend);
        let r = svc.restore(Uuid::new_v4(), false).await.unwrap();
        assert!(matches!(r, RestoreOutcome::Warm(_)));
    }

    #[tokio::test]
    async fn cold_head_when_pending() {
        let m = Manifest::new_pending(
            Uuid::new_v4(),
            "rbd-sessions/s@1",
            "mem/k",
            100,
            true,
            None,
            Uuid::new_v4(),
            vec![],
            "kvm",
            "h",
        );
        // status is Pending by default -> not acceptable for either restore
        let backend = StubBackend::new(Some(m), Some(VolumeId("rbd-sessions/x".into())));
        let svc = RestoreService::new(backend);
        let r = svc.restore(Uuid::new_v4(), true).await.unwrap();
        assert!(matches!(r, RestoreOutcome::ColdHead(_)));
    }

    #[tokio::test]
    async fn local_durable_is_warm_locally_but_not_remotely() {
        let mut m = Manifest::new_pending(
            Uuid::new_v4(),
            "rbd-sessions/s@1",
            "mem/k",
            100,
            false,
            Some(Uuid::new_v4()),
            Uuid::new_v4(),
            vec![],
            "kvm",
            "h",
        );
        m.status = SnapshotStatus::LocalDurable;
        let backend = StubBackend::new(Some(m), Some(VolumeId("rbd-sessions/x".into())));
        let svc = RestoreService::new(backend);

        // Remote restore: LocalDurable is rejected; we get ColdHead.
        let r = svc.restore(Uuid::new_v4(), false).await.unwrap();
        assert!(matches!(r, RestoreOutcome::ColdHead(_)));

        // Local restore: LocalDurable IS acceptable -> Warm.
        let mut m = Manifest::new_pending(
            Uuid::new_v4(),
            "rbd-sessions/s@1",
            "mem/k",
            100,
            false,
            Some(Uuid::new_v4()),
            Uuid::new_v4(),
            vec![],
            "kvm",
            "h",
        );
        m.status = SnapshotStatus::LocalDurable;
        let backend = StubBackend::new(Some(m), Some(VolumeId("rbd-sessions/x".into())));
        let svc = RestoreService::new(backend);
        let r = svc.restore(Uuid::new_v4(), true).await.unwrap();
        assert!(matches!(r, RestoreOutcome::Warm(_)));
    }
}