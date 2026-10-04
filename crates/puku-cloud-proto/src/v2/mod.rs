//! v2 wire protocol: reliability-rebuild extensions to the worker/controld
//! channel. Per docs/RELIABILITY-REBUILD.md §6.
//!
//! `v2` is a parallel module tree, not a replacement for v1: existing
//! messages continue to ride the same WebSocket; new messages get their own
//! envelopes so old workerd builds can ignore them. Each module below maps
//! to one reliability concern.

pub mod fence;
pub mod lease;
pub mod snapshot;
pub mod volume;