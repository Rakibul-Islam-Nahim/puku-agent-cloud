//! Volume abstraction crate.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.1. Replaces direct host-disk access
//! scattered across `puku-workerd` with a single `Volume` trait so that:
//!
//! - workers can be made stateless (R3);
//! - fencing has one implementation, not many;
//! - tests can use the `LocalBackend` without a Ceph cluster.

pub mod error;
pub mod fence;
pub mod local;
pub mod rbd;
pub mod runner;
pub mod traits;
pub mod types;

pub use error::VolumeError;
pub use fence::{Blocklist, FenceAction};
pub use local::{LocalBackend, LocalBackendConfig};
pub use rbd::{RbdBackend, RbdBackendConfig};
pub use runner::{CmdOutput, CommandRunner, ScriptedRunner, SystemRunner};
pub use traits::{Volume, VolumeBackend};
pub use types::{DevicePath, HostId, SnapId, VolumeId};