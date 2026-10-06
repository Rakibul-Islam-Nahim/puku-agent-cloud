//! Control-plane lease sweeper. Runs every second on exactly one controld
//! instance (the caller holds a Postgres advisory lock).
//!
//! Two thresholds, on purpose. Missing renewals makes a host *suspected*
//! quickly (TTL, 3 s): it gets no new work, which costs nothing if it comes
//! back. Declaring it *dead* -- which fences its disks and moves its
//! sessions -- waits a further grace period, because fencing a host that
//! was only briefly unreachable destroys running work for nothing.
//!
//! The mass-loss guard: when a large share of hosts go silent at the same
//! time, the likelier cause is the network or the control plane, not that
//! many machines died at once. Then nobody is declared dead and the sweep
//! reports a hold so an operator is paged instead.

use std::sync::Arc;

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::bmc_probe::BmcProbe;
use crate::types::{LeaseError, LeaseService, LeaseStore};

#[derive(Debug, Clone)]
pub struct SweepPolicy {
    /// How long a host stays suspected before it is declared dead.
    pub dead_after: Duration,
    /// Hold all death declarations when more than this share of live hosts
    /// is suspected...
    pub mass_loss_fraction: f64,
    /// ...and the fleet is at least this big. At least two hosts must be
    /// suspected either way: one silent host is never a mass event.
    pub mass_loss_min_hosts: usize,
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self { dead_after: Duration::seconds(15), mass_loss_fraction: 0.3, mass_loss_min_hosts: 3 }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SweepReport {
    pub newly_suspected: Vec<Uuid>,
    /// Hosts declared dead this tick: recover their sessions (after fencing).
    pub newly_dead: Vec<Uuid>,
    /// Set when the mass-loss guard stopped death declarations this tick.
    pub mass_loss_hold: Option<MassLoss>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MassLoss {
    pub suspected: usize,
    pub live: usize,
}

pub struct LeaseSweeper {
    pub store: Arc<dyn LeaseStore>,
    pub svc: Arc<dyn LeaseService>,
    pub bmc: Arc<dyn BmcProbe>,
    pub policy: SweepPolicy,
}

impl LeaseSweeper {
    pub fn new(store: Arc<dyn LeaseStore>, svc: Arc<dyn LeaseService>, bmc: Arc<dyn BmcProbe>) -> Self {
        Self { store, svc, bmc, policy: SweepPolicy::default() }
    }

    pub fn with_policy(mut self, policy: SweepPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Run one sweep.
    pub async fn sweep_once(&self) -> Result<SweepReport, LeaseError> {
        let now = Utc::now();
        let mut report = SweepReport::default();

        for lease in self.store.list_expiring(now).await? {
            match self.svc.mark_suspected(lease.host_id).await {
                Ok(()) | Err(LeaseError::AlreadySuspected) => report.newly_suspected.push(lease.host_id),
                Err(e) => return Err(e),
            }
        }

        let suspected = self.store.list_suspected().await?;
        let live = self.store.count_live().await?;
        // One silent host is never a mass event, whatever share of the fleet
        // it is: in a fleet of three it is already a third, and holding then
        // would block every failover a small fleet can have.
        if live >= self.policy.mass_loss_min_hosts
            && suspected.len() >= 2
            && suspected.len() as f64 / live.max(1) as f64 > self.policy.mass_loss_fraction
        {
            report.mass_loss_hold = Some(MassLoss { suspected: suspected.len(), live });
            return Ok(report);
        }

        for lease in suspected {
            let since = lease.suspected_at.unwrap_or(lease.expires_at);
            if now - since < self.policy.dead_after {
                continue;
            }
            // A BMC that answers means the machine is powered and its
            // management plane is up: likely a network problem on the data
            // side, not a dead host. Keep it suspected; the fence path will
            // power it off deliberately if an operator decides so.
            if let Some(bmc) = &lease.bmc {
                if self.bmc.is_reachable(bmc).await {
                    continue;
                }
            }
            self.svc.mark_dead(lease.host_id).await?;
            report.newly_dead.push(lease.host_id);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bmc_probe::BmcProbeMock;
    use crate::types::{InMemoryLeaseStore, LeaseServiceImpl, LeaseState};

    struct Fleet {
        store: Arc<InMemoryLeaseStore>,
        svc: Arc<LeaseServiceImpl>,
        sweeper: LeaseSweeper,
    }

    fn fleet(policy: SweepPolicy) -> Fleet {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc = Arc::new(LeaseServiceImpl::new(store.clone(), "controld-1"));
        let sweeper = LeaseSweeper::new(store.clone(), svc.clone(), Arc::new(BmcProbeMock::new(false))).with_policy(policy);
        Fleet { store, svc, sweeper }
    }

    async fn host(f: &Fleet) -> Uuid {
        let h = Uuid::new_v4();
        f.svc.takeover(h, "controld-1").await.unwrap();
        h
    }

    /// Make `h` look silent for `secs` (expired and, if suspected, suspected that long ago).
    async fn age(f: &Fleet, h: Uuid, secs: i64) {
        let mut l = f.store.load(h).await.unwrap().unwrap();
        l.expires_at = Utc::now() - Duration::seconds(secs);
        if l.suspected_at.is_some() {
            l.suspected_at = Some(Utc::now() - Duration::seconds(secs));
        }
        f.store.upsert(&l).await.unwrap();
    }

    fn quick() -> SweepPolicy {
        SweepPolicy { dead_after: Duration::seconds(15), ..SweepPolicy::default() }
    }

    #[tokio::test]
    async fn silent_host_is_suspected_first_not_dead() {
        let f = fleet(quick());
        let hosts = [host(&f).await, host(&f).await, host(&f).await, host(&f).await];
        age(&f, hosts[0], 4).await;
        let r = f.sweeper.sweep_once().await.unwrap();
        assert_eq!(r.newly_suspected, vec![hosts[0]]);
        assert!(r.newly_dead.is_empty(), "suspicion alone must not kill a host");
    }

    #[tokio::test]
    async fn suspected_past_the_grace_period_is_declared_dead() {
        let f = fleet(quick());
        let hosts = [host(&f).await, host(&f).await, host(&f).await, host(&f).await];
        age(&f, hosts[0], 4).await;
        f.sweeper.sweep_once().await.unwrap();
        age(&f, hosts[0], 16).await;
        let r = f.sweeper.sweep_once().await.unwrap();
        assert_eq!(r.newly_dead, vec![hosts[0]]);
        assert_eq!(f.store.load(hosts[0]).await.unwrap().unwrap().state, LeaseState::Released);
    }

    #[tokio::test]
    async fn host_that_comes_back_during_grace_is_not_declared_dead() {
        let f = fleet(quick());
        let hosts = [host(&f).await, host(&f).await, host(&f).await, host(&f).await];
        age(&f, hosts[0], 4).await;
        f.sweeper.sweep_once().await.unwrap();
        let l = f.store.load(hosts[0]).await.unwrap().unwrap();
        f.svc.renew(&l).await.unwrap(); // the network blip is over
        let r = f.sweeper.sweep_once().await.unwrap();
        assert!(r.newly_dead.is_empty());
        assert_eq!(f.store.load(hosts[0]).await.unwrap().unwrap().state, LeaseState::Held);
    }

    #[tokio::test]
    async fn mass_loss_holds_every_death_declaration() {
        let f = fleet(quick());
        let hosts = [host(&f).await, host(&f).await, host(&f).await, host(&f).await];
        // Half the fleet goes silent at once.
        age(&f, hosts[0], 4).await;
        age(&f, hosts[1], 4).await;
        f.sweeper.sweep_once().await.unwrap();
        age(&f, hosts[0], 60).await;
        age(&f, hosts[1], 60).await;
        let r = f.sweeper.sweep_once().await.unwrap();
        assert!(r.newly_dead.is_empty(), "no host declared dead during a mass loss");
        assert_eq!(r.mass_loss_hold, Some(MassLoss { suspected: 2, live: 4 }));
    }

    /// Found on real servers: three hosts, one dies. A third of the fleet is
    /// over the 30% line, but one host is not a mass loss.
    #[tokio::test]
    async fn one_host_of_three_dying_is_declared_dead() {
        let f = fleet(quick());
        let hosts = [host(&f).await, host(&f).await, host(&f).await];
        age(&f, hosts[0], 4).await;
        f.sweeper.sweep_once().await.unwrap();
        age(&f, hosts[0], 16).await;
        let r = f.sweeper.sweep_once().await.unwrap();
        assert_eq!(r.newly_dead, vec![hosts[0]]);
        assert!(r.mass_loss_hold.is_none());
    }

    #[tokio::test]
    async fn small_fleet_losing_one_host_is_not_a_mass_event() {
        let f = fleet(quick());
        let hosts = [host(&f).await, host(&f).await];
        age(&f, hosts[0], 4).await;
        f.sweeper.sweep_once().await.unwrap();
        age(&f, hosts[0], 16).await;
        let r = f.sweeper.sweep_once().await.unwrap();
        assert_eq!(r.newly_dead, vec![hosts[0]]);
        assert!(r.mass_loss_hold.is_none());
    }

    #[tokio::test]
    async fn reachable_bmc_keeps_the_host_suspected() {
        let store = Arc::new(InMemoryLeaseStore::new());
        let svc = Arc::new(LeaseServiceImpl::new(store.clone(), "controld-1"));
        let sweeper = LeaseSweeper::new(store.clone(), svc.clone(), Arc::new(BmcProbeMock::new(true))).with_policy(quick());
        let f = Fleet { store, svc, sweeper };
        let hosts = [host(&f).await, host(&f).await, host(&f).await, host(&f).await];
        let mut l = f.store.load(hosts[0]).await.unwrap().unwrap();
        l.bmc = Some(crate::types::BmcEndpoint {
            hostname: "bmc-0".into(),
            kind: crate::types::BmcKind::Ipmi,
            username: String::new(),
            password: String::new(),
        });
        f.store.upsert(&l).await.unwrap();
        age(&f, hosts[0], 4).await;
        f.sweeper.sweep_once().await.unwrap();
        age(&f, hosts[0], 60).await;
        assert!(f.sweeper.sweep_once().await.unwrap().newly_dead.is_empty());
    }
}
