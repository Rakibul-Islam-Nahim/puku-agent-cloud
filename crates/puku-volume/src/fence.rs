//! Fencing: blocklist-based host isolation, shared by both backends.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.3.2 the hard rule is:
//!
//! > No `recovery.rs` code may call `attach` or `snapshot` for a session
//! > until `Fence::fence` has returned `Ok` for the suspected host.
//!
//! This module is the *primitive* (`blocklist`/`unblocklist`) that both the
//! Ceph backend and the Ceph-via-fence crate use.

use std::sync::Mutex;
use std::collections::HashMap;

use crate::error::VolumeError;
use crate::types::HostId;

/// A fencing action the control plane can request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceAction {
    Blocklist,
    Unblocklist,
    BmcPowerOff,
    BmcPowerCycle,
}

/// In-memory record of who is fenced. Useful for tests; production
/// implementations back this with the Ceph blocklist or BMC.
#[derive(Debug, Default)]
pub struct Blocklist {
    state: Mutex<HashMap<HostId, usize>>,
}

impl Blocklist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Block writes from this host. Idempotent.
    pub fn block(&self, host: HostId) -> Result<(), VolumeError> {
        let mut g = self.state.lock().expect("blocklist poisoned");
        *g.entry(host).or_insert(0) += 1;
        Ok(())
    }

    /// Reverse a previous block. Idempotent.
    pub fn unblock(&self, host: HostId) -> Result<(), VolumeError> {
        let mut g = self.state.lock().expect("blocklist poisoned");
        if let Some(count) = g.get_mut(&host) {
            if *count > 1 {
                *count -= 1;
            } else {
                g.remove(&host);
            }
        }
        Ok(())
    }

    pub fn is_blocked(&self, host: HostId) -> bool {
        self.state.lock().expect("blocklist poisoned").contains_key(&host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn block_then_unblock_clears() {
        let bl = Blocklist::new();
        let h = HostId(Uuid::new_v4());
        bl.block(h).unwrap();
        assert!(bl.is_blocked(h));
        bl.unblock(h).unwrap();
        assert!(!bl.is_blocked(h));
    }

    #[test]
    fn block_is_refcounted() {
        let bl = Blocklist::new();
        let h = HostId(Uuid::new_v4());
        bl.block(h).unwrap();
        bl.block(h).unwrap();
        bl.unblock(h).unwrap();
        assert!(bl.is_blocked(h));
        bl.unblock(h).unwrap();
        assert!(!bl.is_blocked(h));
    }

    #[test]
    fn unblock_unrelated_is_noop() {
        let bl = Blocklist::new();
        let h = HostId(Uuid::new_v4());
        assert!(bl.unblock(h).is_ok());
    }
}