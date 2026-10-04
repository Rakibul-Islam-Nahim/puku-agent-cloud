//! Worker-side heartbeat task. Per docs/RELIABILITY-REBUILD.md §4.2.2.

use std::time::Duration;

use crate::types::{Lease, LeaseError, LeaseService};

/// A long-running heartbeat task. The caller drives it with `tick` once
/// per second; if `renew` fails, the on-miss handler runs (read-only mode,
/// then self-shutdown after 3 consecutive failures).
pub struct Heartbeat {
    pub svc: std::sync::Arc<dyn LeaseService>,
    pub on_miss: Box<dyn FnMut(LeaseError) -> HeartbeatAction + Send>,
    pub consecutive_misses: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatAction {
    /// Lease renewed.
    Ok,
    /// One missed heartbeat; worker should switch to read-only.
    ReadOnly,
    /// Three in a row; worker should self-shutdown the VMs.
    SelfShutdown,
}

impl Heartbeat {
    pub fn new(svc: std::sync::Arc<dyn LeaseService>) -> Self {
        Self {
            svc,
            on_miss: Box::new(|_| HeartbeatAction::ReadOnly),
            consecutive_misses: 0,
        }
    }

    /// Call once per second. The function `renew`s; on failure it walks
    /// the miss counter and invokes the on-miss handler.
    pub async fn tick(&mut self, lease: &Lease) -> HeartbeatAction {
        match self.svc.renew(lease).await {
            Ok(_) => {
                self.consecutive_misses = 0;
                HeartbeatAction::Ok
            }
            Err(e) => {
                self.consecutive_misses += 1;
                (self.on_miss)(e)
            }
        }
    }

    /// Spawn a tokio task that runs the loop. Returns the JoinHandle.
    pub fn spawn(
        mut self,
        mut lease_get: impl FnMut() -> Option<Lease> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(lease) = lease_get() else {
                    continue;
                };
                let _ = self.tick(&lease).await;
            }
        })
    }
}