//! Fencing: make split-brain impossible by cutting the old host's write
//! access before any failover. Per docs/RELIABILITY-REBUILD.md §4.3.
//!
//! ## Operational rule (HARD)
//!
//! No `recovery.rs` code may call `attach` or `snapshot` for a session until
//! `Fence::fence` has returned `Ok` for the suspected host. Enforced by
//! passing the `FenceReceipt` to `volume.attach(...)`.

pub mod audit;
pub mod ceph;
pub mod ipmi;
pub mod redfish;
pub mod types;

pub use audit::{AuditEntry, AuditSink, InMemoryAuditSink};
pub use ceph::CephFencer;
pub use ipmi::IpmiFencer;
pub use redfish::RedfishFencer;
pub use types::{Fence, FenceError, FenceReceipt};