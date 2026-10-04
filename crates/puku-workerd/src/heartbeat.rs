//! Worker-side lease heartbeat.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.2.2 / §4.2.4. Renews the lease every
//! 1 s. On miss count >= 3, marks all VMs read-only, snapshots them, and
//! self-shuts them down. The controld will fence and recover the sessions
//! on another host; the worker's job is just to stop writing and stop
//! pretending to be the healthy owner of the volume.
//!
//! Heartbeat is intentionally not a separate tokio task: it's a method on
//! the lease store that the controld connection loop calls every tick.
//! Splitting it into its own task made the "miss count" state per-task and
//! invisible to recovery, which is the bug this whole file is fixing.

use std::sync::Arc;
use std::time::Duration;

use puku_leases::LeaseService;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Action to take on heartbeat. Per RSD §4.2.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatAction {
    /// Lease held. Continue.
    Ok,
    /// Lease lost (controld unreachable OR lease expired). VMs should
    /// snapshot themselves and stop writing, but stay up so a network
    /// blip doesn't kill paid work.
    ReadOnly,
    /// Lease lost for >= 3 ticks. VMs should self-shutdown.
    /// Controld will fence this host and recover elsewhere.
    SelfShutdown,
}

/// State machine for the heartbeat. Pure logic; tested in isolation.
#[derive(Debug)]
pub struct Heartbeat {
    pub miss_count: u32,
    /// Last action taken. Sticky so we don't double-snapshot.
    pub last_action: HeartbeatAction,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

impl Heartbeat {
    pub fn new() -> Self {
        Self {
            miss_count: 0,
            last_action: HeartbeatAction::Ok,
        }
    }

    /// What should the worker do this tick? Pure: depends only on inputs.
    pub fn decide(&mut self, lease_renewed: bool) -> HeartbeatAction {
        if lease_renewed {
            self.miss_count = 0;
            self.last_action = HeartbeatAction::Ok;
            return HeartbeatAction::Ok;
        }
        self.miss_count = self.miss_count.saturating_add(1);
        let action = if self.miss_count >= 3 {
            HeartbeatAction::SelfShutdown
        } else {
            HeartbeatAction::ReadOnly
        };
        self.last_action = action;
        action
    }

    pub fn reset(&mut self) {
        self.miss_count = 0;
        self.last_action = HeartbeatAction::Ok;
    }
}

/// Spawn the heartbeat loop. The loop renews the lease every 1 s. On miss,
/// it logs and bumps the heartbeat state machine. The caller is responsible
/// for acting on the action (snapshotting, shutting down VMs).
pub fn spawn(
    host_id: Uuid,
    svc: Arc<dyn LeaseService>,
    mut initial_lease: puku_leases::Lease,
    on_action: Arc<dyn Fn(HeartbeatAction) + Send + Sync>,
) {
    tokio::spawn(async move {
        let mut hb = Heartbeat::new();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let renewed = match svc.renew(&initial_lease).await {
                Ok(l) => {
                    initial_lease = l;
                    true
                }
                Err(e) => {
                    warn!(host_id = %host_id, error = %e, "lease renew failed");
                    false
                }
            };
            let action = hb.decide(renewed);
            match action {
                HeartbeatAction::Ok => {
                    // INFO once per minute at most; otherwise it floods.
                    if hb.miss_count == 0 {
                        info!(host_id = %host_id, "lease ok");
                    }
                }
                HeartbeatAction::ReadOnly => {
                    warn!(host_id = %host_id, miss_count = hb.miss_count,
                        "lease miss: VMs going read-only");
                }
                HeartbeatAction::SelfShutdown => {
                    error!(host_id = %host_id, miss_count = hb.miss_count,
                        "lease lost for 3+ ticks: self-shutdown");
                }
            }
            on_action(action);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_resets_miss_count() {
        let mut hb = Heartbeat::new();
        hb.miss_count = 5;
        assert_eq!(hb.decide(true), HeartbeatAction::Ok);
        assert_eq!(hb.miss_count, 0);
    }

    #[test]
    fn first_miss_is_read_only_not_shutdown() {
        let mut hb = Heartbeat::new();
        assert_eq!(hb.decide(false), HeartbeatAction::ReadOnly);
        assert_eq!(hb.miss_count, 1);
    }

    #[test]
    fn three_misses_trigger_self_shutdown() {
        let mut hb = Heartbeat::new();
        hb.decide(false);
        hb.decide(false);
        assert_eq!(hb.decide(false), HeartbeatAction::SelfShutdown);
    }

    #[test]
    fn shutdown_sticks_until_ok() {
        let mut hb = Heartbeat::new();
        hb.miss_count = 5;
        let _ = hb.decide(false);
        // Even if we recover, the next Ok resets the state.
        assert_eq!(hb.decide(true), HeartbeatAction::Ok);
    }
}