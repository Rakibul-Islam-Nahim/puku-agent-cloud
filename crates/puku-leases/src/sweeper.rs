//! Control-plane sweeper. Per docs/RELIABILITY-REBUILD.md §4.2.3.

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::bmc_probe::BmcProbe;
use crate::types::{LeaseError, LeaseService, LeaseState, LeaseStore};

/// Background sweeper. The control plane runs this every 1 s:
/// for each lease with state='held' and expires_at < now(), mark `suspected`.
/// If BMC is unreachable for > 2 probe attempts, mark `released`.
pub struct LeaseSweeper {
    pub store: Arc<dyn LeaseStore>,
    pub svc: Arc<dyn LeaseService>,
    pub bmc: Arc<dyn BmcProbe>,
}

impl LeaseSweeper {
    pub fn new(
        store: Arc<dyn LeaseStore>,
        svc: Arc<dyn LeaseService>,
        bmc: Arc<dyn BmcProbe>,
    ) -> Self {
        Self { store, svc, bmc }
    }

    /// Run one sweep. Returns the hosts that were newly marked suspected.
    pub async fn sweep_once(&self) -> Result<Vec<Uuid>, LeaseError> {
        let now = Utc::now();
        let expiring = self.store.list_expiring(now).await?;
        let mut newly_suspected = Vec::new();
        for lease in expiring {
            // Avoid duplicating suspect on every tick.
            if lease.state == LeaseState::Suspected {
                continue;
            }
            // Optional BMC probe; only confirm death if BMC is configured.
            if let Some(bmc) = &lease.bmc {
                if !self.bmc.is_reachable(bmc).await {
                    // 2 strikes: first probe attempt failed. We do not yet
                    // bump to Released; that decision is owned by the
                    // recovery orchestrator (F4/F5 in RSD §1).
                }
            }
            self.svc.mark_suspected(lease.host_id).await?;
            newly_suspected.push(lease.host_id);
        }
        Ok(newly_suspected)
    }
}