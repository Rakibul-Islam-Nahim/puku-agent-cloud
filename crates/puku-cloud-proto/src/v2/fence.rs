//! v2 fence messages.
//!
//! Per RSD §4.3 / §5.3. Controld -> worker (and back) frames for fencing a
//! suspect host. The actual fence (Ceph blocklist + BMC power-cycle) is
//! performed by controld, not the worker; these are audit + coordination.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FenceRequest {
    pub host_id: Uuid,
    /// Optional BMC info; None when controld has it.
    pub bmc: Option<BmcRef>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BmcRef {
    pub hostname: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FenceReceipt {
    pub host_id: Uuid,
    pub blocklisted: bool,
    pub bmc_action: BmcAction,
    pub audit_id: i64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BmcAction {
    None,
    PoweredOff,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_receipt_serializes_round_trip() {
        let r = FenceReceipt {
            host_id: Uuid::new_v4(),
            blocklisted: true,
            bmc_action: BmcAction::PoweredOff,
            audit_id: 42,
            ts_ms: 1234567890,
        };
        let s = serde_json::to_string(&r).unwrap();
        let parsed: FenceReceipt = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.blocklisted, r.blocklisted);
        assert_eq!(parsed.bmc_action, r.bmc_action);
        assert_eq!(parsed.audit_id, r.audit_id);
    }
}