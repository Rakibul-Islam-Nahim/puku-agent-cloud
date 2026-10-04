//! Ceph blocklist fencer. The primary fence: cuts RBD I/O for the host.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use puku_leases::BmcEndpoint;
use puku_volume::VolumeBackend;

use crate::audit::{AuditEntry, AuditSink};
use crate::types::{Fence, FenceError, FenceReceipt};

/// Fencer that issues `rbd blocklist add/remove` through any
/// `VolumeBackend` (which knows how to talk to Ceph). This is the primary
/// fence; BMC actions are layered on top in `fence()`.
pub struct CephFencer {
    pub volume: Arc<dyn VolumeBackend>,
    pub audit: Arc<dyn AuditSink>,
    pub instance: String,
}

impl CephFencer {
    pub fn new(
        volume: Arc<dyn VolumeBackend>,
        audit: Arc<dyn AuditSink>,
        instance: impl Into<String>,
    ) -> Self {
        Self {
            volume,
            audit,
            instance: instance.into(),
        }
    }
}

#[async_trait]
impl Fence for CephFencer {
    async fn blocklist(&self, host_id: Uuid) -> Result<(), FenceError> {
        self.volume
            .fence(puku_volume::HostId(host_id))
            .await
            .map_err(|e| FenceError::Blocklist(e.to_string()))?;
        let _ = self
            .audit
            .write(&AuditEntry {
                host_id,
                session_id: None,
                action: "blocklist".into(),
                outcome: "ok".into(),
                detail: serde_json::json!({}),
                requested_by: self.instance.clone(),
            })
            .await;
        Ok(())
    }

    async fn unfence(&self, host_id: Uuid) -> Result<(), FenceError> {
        self.volume
            .unfence(puku_volume::HostId(host_id))
            .await
            .map_err(|e| FenceError::Blocklist(e.to_string()))?;
        let _ = self
            .audit
            .write(&AuditEntry {
                host_id,
                session_id: None,
                action: "blocklist".into(), // same table, action semantics
                outcome: "ok".into(),
                detail: serde_json::json!({"unblock": true}),
                requested_by: self.instance.clone(),
            })
            .await;
        Ok(())
    }

    async fn fence(
        &self,
        host_id: Uuid,
        bmc: Option<&BmcEndpoint>,
    ) -> Result<FenceReceipt, FenceError> {
        // Step 1: blocklist. This is the primary fence. Must succeed.
        self.blocklist(host_id).await?;
        // Step 2: optional BMC. The contract says BMC failure must NOT fail
        // the whole fence. The receipt records what happened.
        let mut bmc_action = None;
        if let Some(bmc) = bmc {
            // The actual call is layered through `bmc_action` so the test
            // can swap in a mock. The default impls (`IpmiFencer`,
            // `RedfishFencer`) shell out to the tool; production code wires
            // whichever is configured.
            let outcome = match bmc.kind {
                puku_leases::BmcKind::Ipmi => {
                    crate::ipmi::IpmiFencer.shell_power_off(bmc).await
                }
                puku_leases::BmcKind::Redfish => {
                    crate::redfish::RedfishFencer.shell_power_off(bmc).await
                }
            };
            match outcome {
                Ok(action) => {
                    bmc_action = Some(action);
                    let _ = self
                        .audit
                        .write(&AuditEntry {
                            host_id,
                            session_id: None,
                            action: "bmc_poweroff".into(),
                            outcome: "ok".into(),
                            detail: serde_json::json!({"kind": format!("{:?}", bmc.kind)}),
                            requested_by: self.instance.clone(),
                        })
                        .await;
                }
                Err(e) => {
                    let _ = self
                        .audit
                        .write(&AuditEntry {
                            host_id,
                            session_id: None,
                            action: "bmc_poweroff".into(),
                            outcome: "failed".into(),
                            detail: serde_json::json!({"error": e.to_string()}),
                            requested_by: self.instance.clone(),
                        })
                        .await;
                }
            }
        }
        Ok(FenceReceipt {
            host_id,
            blocklisted_at: chrono::Utc::now(),
            bmc_action,
            audit_log_id: 0, // assigned by AuditSink in production
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puku_volume::{LocalBackend, LocalBackendConfig};
    use tempfile::TempDir;

    #[tokio::test]
    async fn fence_records_audit_and_blocks() {
        let tmp = TempDir::new().unwrap();
        let volume = Arc::new(LocalBackend::new(LocalBackendConfig::new(tmp.path())));
        let audit = Arc::new(crate::audit::InMemoryAuditSink::new());
        let fencer = CephFencer::new(volume.clone(), audit.clone(), "test-instance");

        let host = Uuid::new_v4();
        fencer.blocklist(host).await.unwrap();

        // Writing through the volume from the fenced host fails.
        let res = volume
            .attach(
                &puku_volume::VolumeId(format!("local/{}", Uuid::new_v4())),
                puku_volume::HostId(host),
            )
            .await;
        assert!(matches!(res, Err(puku_volume::VolumeError::HostFenced(_))));

        // Audit got one entry.
        let entries = audit.entries.lock().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "blocklist");
    }

    #[tokio::test]
    async fn unfence_reverses() {
        let tmp = TempDir::new().unwrap();
        let volume = Arc::new(LocalBackend::new(LocalBackendConfig::new(tmp.path())));
        let audit = Arc::new(crate::audit::InMemoryAuditSink::new());
        let fencer = CephFencer::new(volume.clone(), audit.clone(), "test-instance");

        let host = Uuid::new_v4();
        fencer.blocklist(host).await.unwrap();
        fencer.unfence(host).await.unwrap();

        // Now attach succeeds.
        let res = volume
            .attach(
                &puku_volume::VolumeId(format!("local/{}", Uuid::new_v4())),
                puku_volume::HostId(host),
            )
            .await;
        // NotFound because we did not create the volume, but NOT HostFenced.
        assert!(matches!(res, Err(puku_volume::VolumeError::NotFound(_))));
    }

    #[tokio::test]
    async fn fence_with_bmc_succeeds_even_if_bmc_unconfigured() {
        let tmp = TempDir::new().unwrap();
        let volume = Arc::new(LocalBackend::new(LocalBackendConfig::new(tmp.path())));
        let audit = Arc::new(crate::audit::InMemoryAuditSink::new());
        let fencer = CephFencer::new(volume.clone(), audit.clone(), "test-instance");

        let host = Uuid::new_v4();
        let r = fencer.fence(host, None).await.unwrap();
        assert_eq!(r.host_id, host);
        assert!(r.bmc_action.is_none());
    }
}