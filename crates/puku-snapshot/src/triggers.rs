//! Snapshot triggers — single source of truth for the RPO interval.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.4.3.
//!
//! ## Why one constant
//!
//! RPO lives in code as a constant keyed on `sla_tier`. There is no
//! `rpo_target_s` column to drift out of sync with the promise.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlaTier {
    Standard,
    Premium,
    BestEffort,
}

impl SlaTier {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(Self::Standard),
            "premium" => Some(Self::Premium),
            "best_effort" => Some(Self::BestEffort),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryMode {
    ColdOnly,
    WarmAllowed,
}

/// One function decides the effective mode. Nothing else may decide it.
/// (RSD §4.4.3.)
pub fn effective_recovery_mode(
    sla: SlaTier,
    session_pref: RecoveryMode,
    org_ceiling: RecoveryMode,
    worker_warm_ok: bool,
) -> RecoveryMode {
    use RecoveryMode::*;
    if sla == SlaTier::BestEffort || !worker_warm_ok {
        return ColdOnly;
    }
    // Never above the org ceiling; session can only lower it.
    if session_pref == ColdOnly || org_ceiling == ColdOnly {
        ColdOnly
    } else {
        WarmAllowed
    }
}

/// RPO per tier (RSD §4.4.3). `None` means cold_only sessions -- their
/// RAM RPO is undefined by design.
pub fn rpo_seconds(tier: SlaTier) -> Option<u32> {
    match tier {
        SlaTier::Premium => Some(5),
        SlaTier::Standard => Some(60),
        SlaTier::BestEffort => None,
    }
}

/// What the trigger decided. Used by the capture path (and by tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerDecision {
    /// Not time yet; do nothing.
    Skip,
    /// Tier doesn't take RAM snapshots; do nothing.
    ColdOnly,
    /// Take a paired snapshot now.
    Snapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_effort_is_always_cold() {
        assert_eq!(
            effective_recovery_mode(
                SlaTier::BestEffort,
                RecoveryMode::WarmAllowed,
                RecoveryMode::WarmAllowed,
                true
            ),
            RecoveryMode::ColdOnly
        );
    }

    #[test]
    fn worker_warm_off_forces_cold() {
        assert_eq!(
            effective_recovery_mode(
                SlaTier::Premium,
                RecoveryMode::WarmAllowed,
                RecoveryMode::WarmAllowed,
                false
            ),
            RecoveryMode::ColdOnly
        );
    }

    #[test]
    fn org_ceiling_wins() {
        assert_eq!(
            effective_recovery_mode(
                SlaTier::Premium,
                RecoveryMode::WarmAllowed,
                RecoveryMode::ColdOnly,
                true
            ),
            RecoveryMode::ColdOnly
        );
    }

    #[test]
    fn session_can_lower() {
        assert_eq!(
            effective_recovery_mode(
                SlaTier::Premium,
                RecoveryMode::ColdOnly,
                RecoveryMode::WarmAllowed,
                true
            ),
            RecoveryMode::ColdOnly
        );
    }

    #[test]
    fn otherwise_warm() {
        assert_eq!(
            effective_recovery_mode(
                SlaTier::Standard,
                RecoveryMode::WarmAllowed,
                RecoveryMode::WarmAllowed,
                true
            ),
            RecoveryMode::WarmAllowed
        );
    }

    #[test]
    fn rpo_per_tier() {
        assert_eq!(rpo_seconds(SlaTier::Premium), Some(5));
        assert_eq!(rpo_seconds(SlaTier::Standard), Some(60));
        assert_eq!(rpo_seconds(SlaTier::BestEffort), None);
    }
}