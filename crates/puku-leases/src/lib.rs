//! Lease service: 1–2 s heartbeat, expiry sweeper.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.2.
//!
//! ## Worker-side contract
//!
//! ```rust,ignore
//! loop {
//!     lease_service.renew(&lease).await?;
//!     tokio::time::sleep(Duration::from_secs(1)).await;
//! }
//! ```
//!
//! If `renew` fails (controld unreachable, advisory lock contention), the
//! worker MUST:
//!  1. log `lease_lost` with `host_id`
//!  2. set its VMs to read-only (snap a snapshot, then stop writing)
//!  3. continue trying; if N=3 consecutive failures, self-shutdown the VMs
//!
//! ## Sweeper contract
//!
//! The control plane runs the sweeper every 1 s:
//!
//! ```rust,ignore
//! for lease in leases WHERE state = 'held' AND expires_at < now() {
//!     mark_suspected(lease.host_id).await?;
//!     // Optionally probe BMC to confirm death.
//!     // If BMC unreachable for > 2 probe attempts -> mark confirmed dead.
//! }
//! ```

pub mod bmc_probe;
pub mod heartbeat;
pub mod sweeper;
pub mod types;

pub use bmc_probe::BmcProbe;
pub use heartbeat::Heartbeat;
pub use sweeper::LeaseSweeper;
pub use types::{
    BmcEndpoint, BmcKind, Lease, LeaseError, LeaseService, LeaseServiceImpl, LeaseState, LeaseStore,
};
