//! Recovery orchestrator.
//!
//! Per docs/RELIABILITY-REBUILD.md §5.1. Decides tier-1 vs tier-2 recovery,
//! drives fencing (always before any cross-host relocate), and chooses
//! between warm/cold resume from the snapshot subsystem.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use puku_fence::Fence;
use puku_snapshot::RestoreOutcome;
use puku_volume::VolumeBackend;

/// Recovery tier (NOT the SLA tier -- see RSD §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryChoice {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
pub enum CrashReason {
    VmmExit { code: Option<i32> },
    GuestHang,
    HeartbeatTimeout,
    HostDead,
    NetworkPartition,
    OperatorRequest,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("session not found")]
    SessionNotFound,
    #[error("recovery failed: {0}")]
    RecoverFailed(String),
}

/// Session snapshot, used to decide tier. In production this is the
/// `sessions` row; here a thin DTO is enough for the decision logic.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub id: Uuid,
    pub worker_id: Option<Uuid>,
    pub state: String,
    pub desired_state: String,
    pub recovery_mode: String,
    pub sla_tier: String,
    pub last_snapshot_id: Option<String>,
    pub snapshot_host_id: Option<Uuid>,
    pub crash_count: i32,
}

/// Step 1: tier decision. Per RSD §5.1.2.
///
/// Local = same host, RBD already mapped, no fence needed.
/// Remote = worker is suspect or session evicted.
pub fn decide_recovery(session: &SessionSnapshot, worker_alive: bool) -> RecoveryChoice {
    // Crash-loop eviction: 3+ crashes in the rolling window -> Remote.
    if session.crash_count >= 3 {
        return RecoveryChoice::Remote;
    }
    if !worker_alive {
        return RecoveryChoice::Remote;
    }
    // desired=stopped + observed=missing = expected shutdown, not recovery.
    // (The orchestrator short-circuits before reaching here, but the
    // function still respects it for safety.)
    if session.desired_state == "stopped" {
        return RecoveryChoice::Local; // harmless; no recovery happens
    }
    RecoveryChoice::Local
}

/// Pluggable restore backend the recovery orchestrator uses. Wired against
/// `RestoreService` from `puku-snapshot`. The wrapper trait here is what
/// `recover_session` depends on.
#[async_trait]
pub trait RestoreDriver: Send + Sync {
    async fn pick_manifest(
        &self,
        session_id: Uuid,
        prefer_local: bool,
    ) -> Result<Option<puku_snapshot::Manifest>, String>;
    async fn current_volume(
        &self,
        session_id: Uuid,
    ) -> Result<Option<puku_volume::VolumeId>, String>;
}

/// `recover_session`: fence BEFORE attach on a different host, then call
/// the snapshot subsystem for the restore.
pub async fn recover_session(
    session: SessionSnapshot,
    choice: RecoveryChoice,
    fence: Arc<dyn Fence>,
    driver: Arc<dyn RestoreDriver>,
    _volume: Arc<dyn VolumeBackend>,
    worker_alive: bool,
) -> Result<RecoveryOutcome, RecoveryError> {
    // Step 2: fence if remote. FenceReceipt is the precondition for any
    // cross-host relocate; passing it through makes the rule load-bearing.
    if choice == RecoveryChoice::Remote {
        let host = session
            .worker_id
            .ok_or_else(|| RecoveryError::RecoverFailed("no worker_id for remote fence".into()))?;
        // The session's disk is what the old host could still write to, so
        // that is what gets cut off -- before anything else touches it.
        // A fence that fails stops the recovery here: never attach
        // half-fenced. BMC is not passed yet; the storage fence is primary.
        let volumes: Vec<puku_volume::VolumeId> =
            driver.current_volume(session.id).await.map_err(RecoveryError::RecoverFailed)?.into_iter().collect();
        fence
            .fence_volumes(host, Some(session.id), &volumes, None)
            .await
            .map_err(|e| RecoveryError::RecoverFailed(format!("fence failed, recovery stopped: {e}")))?;
    }
    let _ = worker_alive; // already used in decide_recovery

    // Step 3: drive the restore. prefer_local=true for Local (the worker
    // holds LocalDurable bytes if it just rebooted; this is the cheap path).
    let prefer_local = matches!(choice, RecoveryChoice::Local);
    let outcome = driver.pick_manifest(session.id, prefer_local).await;
    let outcome = match outcome {
        Ok(Some(m)) => {
            let usable = if prefer_local {
                m.status.acceptable_for_local_restore()
            } else {
                m.status.acceptable_for_remote_restore()
            };
            if usable {
                RestoreOutcome::Warm(m)
            } else {
                match driver.current_volume(session.id).await {
                    Ok(Some(v)) => RestoreOutcome::ColdHead(v),
                    Ok(None) => {
                        return Err(RecoveryError::RecoverFailed(
                            "no manifest and no volume".into(),
                        ));
                    }
                    Err(e) => return Err(RecoveryError::RecoverFailed(e)),
                }
            }
        }
        Ok(None) => match driver.current_volume(session.id).await {
            Ok(Some(v)) => RestoreOutcome::ColdHead(v),
            Ok(None) => {
                return Err(RecoveryError::RecoverFailed("no manifest and no volume".into()));
            }
            Err(e) => return Err(RecoveryError::RecoverFailed(e)),
        },
        Err(e) => return Err(RecoveryError::RecoverFailed(e)),
    };
    Ok(RecoveryOutcome {
        kind: outcome.kind().to_string(),
        session_id: session.id,
    })
}

#[derive(Debug, Clone)]
pub struct RecoveryOutcome {
    pub kind: String,
    pub session_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(crash: i32, desired: &str) -> SessionSnapshot {
        SessionSnapshot {
            id: Uuid::new_v4(),
            worker_id: Some(Uuid::new_v4()),
            state: "running".into(),
            desired_state: desired.into(),
            recovery_mode: "warm_allowed".into(),
            sla_tier: "standard".into(),
            last_snapshot_id: None,
            snapshot_host_id: None,
            crash_count: crash,
        }
    }

    #[test]
    fn healthy_worker_is_local() {
        assert_eq!(decide_recovery(&sess(0, "running"), true), RecoveryChoice::Local);
    }

    #[test]
    fn dead_worker_is_remote() {
        assert_eq!(decide_recovery(&sess(0, "running"), false), RecoveryChoice::Remote);
    }

    #[test]
    fn crash_loop_evicts() {
        assert_eq!(decide_recovery(&sess(3, "running"), true), RecoveryChoice::Remote);
        assert_eq!(decide_recovery(&sess(5, "running"), true), RecoveryChoice::Remote);
    }

    #[test]
    fn desired_stopped_short_circuits() {
        assert_eq!(decide_recovery(&sess(0, "stopped"), true), RecoveryChoice::Local);
    }

    /// Records the order of operations so tests can assert fence-first.
    #[derive(Default)]
    struct Trace(std::sync::Mutex<Vec<String>>);
    impl Trace {
        fn push(&self, s: impl Into<String>) {
            self.0.lock().unwrap().push(s.into());
        }
        fn get(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeFence {
        trace: Arc<Trace>,
        fail: bool,
    }

    #[async_trait]
    impl Fence for FakeFence {
        async fn blocklist(&self, _h: Uuid) -> Result<(), puku_fence::FenceError> {
            Ok(())
        }
        async fn fence(&self, _h: Uuid, _b: Option<&puku_leases::BmcEndpoint>) -> Result<puku_fence::FenceReceipt, puku_fence::FenceError> {
            unreachable!("recovery must fence volumes, not just the host")
        }
        async fn unfence(&self, _h: Uuid) -> Result<(), puku_fence::FenceError> {
            Ok(())
        }
        async fn fence_volumes(
            &self,
            host_id: Uuid,
            _s: Option<Uuid>,
            volumes: &[puku_volume::VolumeId],
            _b: Option<&puku_leases::BmcEndpoint>,
        ) -> Result<puku_fence::FenceReceipt, puku_fence::FenceError> {
            self.trace.push(format!("fence {}", volumes.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")));
            if self.fail {
                return Err(puku_fence::FenceError::Blocklist("denied".into()));
            }
            Ok(puku_fence::FenceReceipt {
                host_id,
                blocklisted_at: chrono::Utc::now(),
                bmc_action: None,
                audit_log_id: 1,
                volumes: volumes.iter().map(|v| v.to_string()).collect(),
                clients: vec![],
            })
        }
    }

    struct FakeDriver {
        trace: Arc<Trace>,
    }

    #[async_trait]
    impl RestoreDriver for FakeDriver {
        async fn pick_manifest(&self, _s: Uuid, _l: bool) -> Result<Option<puku_snapshot::Manifest>, String> {
            self.trace.push("pick_manifest");
            Ok(None)
        }
        async fn current_volume(&self, s: Uuid) -> Result<Option<puku_volume::VolumeId>, String> {
            self.trace.push("current_volume");
            Ok(Some(puku_volume::VolumeId(format!("puku-sessions/{s}"))))
        }
    }

    fn volume() -> Arc<dyn VolumeBackend> {
        Arc::new(puku_volume::RbdBackend::for_test())
    }

    #[tokio::test]
    async fn remote_recovery_fences_the_volume_before_restoring() {
        let trace = Arc::new(Trace::default());
        let s = sess(0, "running");
        let fence = Arc::new(FakeFence { trace: trace.clone(), fail: false });
        let driver = Arc::new(FakeDriver { trace: trace.clone() });
        let out = recover_session(s.clone(), RecoveryChoice::Remote, fence, driver, volume(), false).await.unwrap();
        assert_eq!(out.kind, "ColdHead");
        let t = trace.get();
        let fence_at = t.iter().position(|x| x.starts_with("fence ")).expect("fenced");
        let pick_at = t.iter().position(|x| x == "pick_manifest").expect("restored");
        assert!(fence_at < pick_at, "fence must come before any restore: {t:?}");
        assert_eq!(t[fence_at], format!("fence puku-sessions/{}", s.id));
    }

    #[tokio::test]
    async fn failed_fence_stops_recovery() {
        let trace = Arc::new(Trace::default());
        let fence = Arc::new(FakeFence { trace: trace.clone(), fail: true });
        let driver = Arc::new(FakeDriver { trace: trace.clone() });
        let res = recover_session(sess(0, "running"), RecoveryChoice::Remote, fence, driver, volume(), false).await;
        assert!(matches!(res, Err(RecoveryError::RecoverFailed(m)) if m.contains("fence failed")));
        assert!(!trace.get().iter().any(|x| x == "pick_manifest"), "nothing restored after a failed fence");
    }

    #[tokio::test]
    async fn local_recovery_does_not_fence() {
        let trace = Arc::new(Trace::default());
        let fence = Arc::new(FakeFence { trace: trace.clone(), fail: true });
        let driver = Arc::new(FakeDriver { trace: trace.clone() });
        recover_session(sess(0, "running"), RecoveryChoice::Local, fence, driver, volume(), true).await.unwrap();
        assert!(!trace.get().iter().any(|x| x.starts_with("fence ")));
    }
}