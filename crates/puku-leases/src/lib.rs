//! Host liveness leases. Per docs/RELIABILITY-REBUILD.md §4.2, with the
//! review corrections:
//!
//! - **The worker never stops its own VMs.** It sends a renew frame every
//!   second on its existing control link; controld renews the lease. If the
//!   link or the control plane is down, the worker keeps running: a
//!   control-plane outage must not become a data-plane outage. Stopping a
//!   host's writes is the storage fence's job, and only controld orders it.
//! - **Two thresholds.** Missed renewals make a host *suspected* after the
//!   TTL (3 s): no new work. It is declared *dead* only after a further
//!   grace period (15 s by default), and never during a mass loss.
//! - **Generation per takeover.** Each registration starts a new
//!   generation; a host that comes back after being declared dead must
//!   re-register before it can run anything.
//!
//! ## Sweeper contract
//!
//! One controld instance at a time (Postgres advisory lock) runs
//! [`LeaseSweeper::sweep_once`] every second and acts on its
//! [`SweepReport`]: suspected hosts get no placements, dead hosts have their
//! sessions recovered after fencing, a mass-loss hold pages an operator.

pub mod bmc_probe;
pub mod sweeper;
pub mod types;

pub use bmc_probe::BmcProbe;
pub use sweeper::{LeaseSweeper, MassLoss, SweepPolicy, SweepReport};
pub use types::{
    BmcEndpoint, BmcKind, InMemoryLeaseStore, Lease, LeaseError, LeaseService, LeaseServiceImpl, LeaseState,
    LeaseStore, LEASE_TTL_SECONDS,
};
