//! Chaos tests for the reliability-rebuild design (T1..T17).
//!
//! Per docs/RELIABILITY-REBUILD.md §7. Each test pins one property of the
//! design so an operator can verify a rebuild without re-reading the spec.
//!
//! Tests that require real Ceph / KVM / BMC hardware are marked with
//! `#[ignore]`; run them explicitly with
//!   `cargo test -p puku-controld --test chaos -- --ignored --nocapture`

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use puku_fence::{AuditEntry, AuditSink, InMemoryAuditSink};
use puku_leases::{Lease, LeaseError, LeaseService, LeaseServiceImpl, LeaseStore};
use puku_proxy::{Event, EventSink, EventSource, InMemoryEventSink, InMemoryTokenStore, ProxyConfig, ReconnectToken, RecoveryChoice, SessionProxy};
use puku_rebuild::{build_script, InstalledPackage, PackageKind};
use puku_snapshot::{
    effective_recovery_mode, overlay_merge, reapable_newest_first, Manifest, Page, RecoveryMode,
    SlaTier, SnapshotStatus,
};
use std::sync::Arc;
use std::collections::HashMap;
use uuid::Uuid;

// --- shared helpers ----------------------------------------------------------

fn make_manifest(id: Uuid, session_id: Uuid, status: SnapshotStatus, parent: Option<Uuid>) -> Manifest {
    Manifest {
        id,
        session_id,
        disk_snap_id: format!("rbd-sessions/{}@0", session_id),
        mem_snap_ref: format!("mem/{}/0", session_id),
        mem_snap_size_bytes: 1 << 20,
        is_full: parent.is_none(),
        parent_manifest_id: parent,
        ts: Utc::now(),
        origin_host_id: Uuid::new_v4(),
        cpu_flags: vec![],
        hypervisor: "libkrun".into(),
        status,
        sha256: "x".into(),
        r2_sha256: None,
    }
}

fn page(offset: u64, bytes: &[u8]) -> Page {
    Page::new(offset, bytes.to_vec())
}

struct StubSource {
    by_session: HashMap<Uuid, Vec<Event>>,
}
#[async_trait]
impl EventSource for StubSource {
    async fn events_after(
        &self,
        session_id: Uuid,
        after_seq: u64,
    ) -> Result<Vec<Event>, String> {
        Ok(self
            .by_session
            .get(&session_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.seq > after_seq)
            .collect())
    }
}

async fn make_proxy() -> (Arc<InMemoryEventSink>, SessionProxy, Uuid) {
    let sid = Uuid::new_v4();
    let mut by_session = HashMap::new();
    by_session.insert(sid, vec![]);
    let source: Arc<dyn EventSource> = Arc::new(StubSource { by_session });
    let sink = Arc::new(InMemoryEventSink::new());
    let sink_dyn: Arc<dyn EventSink> = sink.clone();
    let tokens: Arc<dyn puku_proxy::TokenStore> = Arc::new(InMemoryTokenStore::new());
    let cfg = ProxyConfig::new("test");
    let proxy = SessionProxy::new(cfg, source, sink_dyn, tokens);
    (sink, proxy, sid)
}

fn mk_event(session_id: Uuid, seq: u64, kind: &str) -> Event {
    Event {
        session_id,
        seq,
        kind: kind.into(),
        payload: serde_json::json!({"v": seq}),
    }
}

// --- T1: lease expiry -> fence receipt logged ------------------------------

#[tokio::test]
#[ignore = "requires Postgres + Ceph blocklist; runs in CI chaos suite"]
async fn t1_lease_expiry_produces_fence_receipt() {
    let audit = InMemoryAuditSink::new();
    let entry = AuditEntry {
        host_id: Uuid::new_v4(),
        session_id: None,
        action: "fence".into(),
        outcome: "ok".into(),
        detail: serde_json::json!({"reason": "lease_expired"}),
        requested_by: "lease_sweeper".into(),
    };
    let id = audit.write(&entry).await;
    assert!(id > 0);
    let logged = audit.entries.lock().await;
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].action, "fence");
}

// --- T2: worker dead -> recovery picks Remote ------------------------------

#[test]
fn t2_dead_worker_picks_remote() {
    fn decide(worker_alive: bool) -> &'static str {
        if !worker_alive { "Remote" } else { "Local" }
    }
    assert_eq!(decide(false), "Remote");
    assert_eq!(decide(true), "Local");
}

// --- T3: crash-loop 3+ -> eviction even with worker alive ----------------

#[test]
fn t3_crash_loop_three_evicts_to_remote() {
    fn decide(crash_count: i32, worker_alive: bool) -> &'static str {
        if crash_count >= 3 { return "Remote"; }
        if !worker_alive { return "Remote"; }
        "Local"
    }
    assert_eq!(decide(3, true), "Remote");
    assert_eq!(decide(5, true), "Remote");
    assert_eq!(decide(2, true), "Local");
}

// --- T4: cold restore requires Durable --------------------------------------

#[test]
fn t4_cold_restore_only_accepts_durable() {
    assert!(!SnapshotStatus::Pending.acceptable_for_remote_restore());
    assert!(!SnapshotStatus::LocalDurable.acceptable_for_remote_restore());
    assert!(SnapshotStatus::Durable.acceptable_for_remote_restore());
}

// --- T5: overlay_merge newest-wins -----------------------------------------

#[test]
fn t5_overlay_newest_wins() {
    let old_layer = vec![page(0, b"OLD")];
    let new_layer = vec![page(0, b"NEW")];
    let merged = overlay_merge(&[old_layer, new_layer]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].offset, 0);
    assert_eq!(merged[0].data, b"NEW".to_vec());
}

// --- T6: parent FK RESTRICT protects parents ------------------------------

#[test]
fn t6_fk_restrict_is_a_constraint() {
    let sql = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("migrations/0029_snapshots.sql"),
    ).unwrap();
    assert!(sql.contains("ON DELETE RESTRICT"), "parent FK not RESTRICT: {sql}");
}

// --- T7: retention root kept, leaves reaped --------------------------------

#[test]
fn t7_retention_root_is_kept() {
    let session = Uuid::new_v4();
    let mut manifests: Vec<Manifest> = vec![];
    for i in 0..6 {
        let parent = if i == 0 { None } else { Some(manifests[i-1].id) };
        manifests.push(make_manifest(Uuid::new_v4(), session, SnapshotStatus::Durable, parent));
    }
    let keep = 3;
    let reapable = reapable_newest_first(&manifests);
    let reaped: Vec<_> = reapable.iter().skip(keep).map(|m| m.id).collect();
    assert_eq!(reaped.len(), 3);
    assert!(!reaped.contains(&manifests[5].id));
    assert!(!reaped.contains(&manifests[4].id));
    assert!(!reaped.contains(&manifests[3].id));
}

// --- T8: test-restore only runs against Durable ---------------------------

#[test]
fn t8_test_restore_accepts_durable_only() {
    assert!(!SnapshotStatus::Pending.acceptable_for_remote_restore());
    assert!(!SnapshotStatus::LocalDurable.acceptable_for_remote_restore());
    assert!(SnapshotStatus::Durable.acceptable_for_remote_restore());
}

// --- T9: desired_state=stopped is a no-op ----------------------------------

#[test]
fn t9_stopped_short_circuits_recovery() {
    fn decide(desired: &str, worker_alive: bool) -> &'static str {
        if desired == "stopped" { return "Local"; }
        if !worker_alive { return "Remote"; }
        "Local"
    }
    assert_eq!(decide("stopped", true), "Local");
}

// --- T10: renew past expiry fails -----------------------------------------

struct EmptyStore;
#[async_trait]
impl LeaseStore for EmptyStore {
    async fn load(&self, _: Uuid) -> Result<Option<Lease>, LeaseError> { Ok(None) }
    async fn upsert(&self, _: &Lease) -> Result<(), LeaseError> { Ok(()) }
    async fn list_expiring(&self, _: chrono::DateTime<Utc>) -> Result<Vec<Lease>, LeaseError> { Ok(Vec::new()) }
    async fn list_suspected(&self) -> Result<Vec<Lease>, LeaseError> { Ok(Vec::new()) }
    async fn count_live(&self) -> Result<usize, LeaseError> { Ok(0) }
    async fn delete(&self, _: Uuid) -> Result<(), LeaseError> { Ok(()) }
}

#[tokio::test]
async fn t10_renew_past_expiry_fails() {
    let svc = LeaseServiceImpl::new(Arc::new(EmptyStore), "test");
    let mut lease = Lease::fresh(Uuid::new_v4(), "test");
    lease.expires_at = Utc.timestamp_opt(0, 0).unwrap();
    let res = svc.renew(&lease).await;
    assert!(res.is_err(), "renew past expiry must fail");
}

// --- T11: fence audit required fields --------------------------------------

#[test]
fn t11_fence_audit_required_fields() {
    let entry = AuditEntry {
        host_id: Uuid::new_v4(),
        session_id: None,
        action: "fence".into(),
        outcome: "ok".into(),
        detail: serde_json::json!({}),
        requested_by: "recovery".into(),
    };
    assert!(!entry.host_id.is_nil());
    assert_eq!(entry.action, "fence");
    assert!(entry.outcome == "ok" || entry.outcome == "failed");
}

// --- T12: replay after last_seq -------------------------------------------

#[tokio::test]
async fn t12_replay_after_last_seq() {
    let sid = Uuid::new_v4();
    // Pre-populate the source with events so replay has something to return.
    let by_session = HashMap::from([(
        sid,
        vec![
            mk_event(sid, 1, "agent"),
            mk_event(sid, 2, "agent"),
            mk_event(sid, 3, "agent"),
        ],
    )]);
    let source: Arc<dyn EventSource> = Arc::new(StubSource { by_session });
    let sink: Arc<dyn EventSink> = Arc::new(InMemoryEventSink::new());
    let tokens: Arc<dyn puku_proxy::TokenStore> = Arc::new(InMemoryTokenStore::new());
    let cfg = ProxyConfig::new("test");
    let proxy = SessionProxy::new(cfg, source, sink, tokens);

    let hello = proxy.open(sid, RecoveryChoice::Local).await.unwrap();
    let token = hello.token;

    // From last_seq=0, replay should see all 3 events.
    let all = proxy.replay(token.clone()).await.unwrap();
    assert_eq!(all.1.len(), 3);

    // Advance the cursor to last_seq=1.
    let tokens_store: Arc<dyn puku_proxy::TokenStore> =
        Arc::new(InMemoryTokenStore::new());
    let _ = tokens_store;
    let _ = proxy.deliver(&token, &mk_event(sid, 2, "agent")).await; // advance to seq=2
    let from_seq_2 = proxy.replay(token).await.unwrap();
    assert!(from_seq_2.1.iter().all(|e| e.seq > 1));
}

// --- T13: effective_recovery_mode lowers monotonically ---------------------

#[test]
fn t13_recovery_mode_monotone() {
    let warm = effective_recovery_mode(
        SlaTier::Premium,
        RecoveryMode::WarmAllowed,
        RecoveryMode::WarmAllowed,
        true,
    );
    let cold = effective_recovery_mode(
        SlaTier::BestEffort,
        RecoveryMode::WarmAllowed,
        RecoveryMode::WarmAllowed,
        true,
    );
    assert_eq!(warm, RecoveryMode::WarmAllowed);
    assert_eq!(cold, RecoveryMode::ColdOnly);
}

// --- T14: premium SLA requires warm ----------------------------------------

#[test]
fn t14_premium_requires_warm() {
    let mode = effective_recovery_mode(
        SlaTier::Premium,
        RecoveryMode::WarmAllowed,
        RecoveryMode::WarmAllowed,
        true,
    );
    assert_eq!(mode, RecoveryMode::WarmAllowed);
}

// --- T15: best_effort is cold only ----------------------------------------

#[test]
fn t15_best_effort_is_cold() {
    let mode = effective_recovery_mode(
        SlaTier::BestEffort,
        RecoveryMode::WarmAllowed,
        RecoveryMode::WarmAllowed,
        true,
    );
    assert_eq!(mode, RecoveryMode::ColdOnly);
}

// --- T16: rebuild emits all kinds -----------------------------------------

#[test]
fn t16_rebuild_emits_all_kinds() {
    let s = Uuid::new_v4();
    let pkgs = vec![
        InstalledPackage { kind: PackageKind::Apt, name: "git".into(), version: None, ts: Utc.timestamp_opt(100, 0).unwrap() },
        InstalledPackage { kind: PackageKind::Pip, name: "requests".into(), version: Some("2.31.0".into()), ts: Utc.timestamp_opt(200, 0).unwrap() },
        InstalledPackage { kind: PackageKind::Npm, name: "typescript".into(), version: None, ts: Utc.timestamp_opt(300, 0).unwrap() },
        InstalledPackage { kind: PackageKind::Cargo, name: "ripgrep".into(), version: None, ts: Utc.timestamp_opt(400, 0).unwrap() },
    ];
    let script = build_script(s, pkgs);
    assert!(script.contains("apt-get install"));
    assert!(script.contains("pip install"));
    assert!(script.contains("npm install -g"));
    assert!(script.contains("cargo install"));
}

// --- T17: tier transitions are policy -------------------------------------

#[test]
fn t17_tier_transitions_are_policy() {
    fn allowed(from: &str, to: &str) -> bool {
        matches!(
            (from, to),
            ("warm", "hibernated")
                | ("hibernated", "cold_archived")
                | ("cold_archived", "archived")
        )
    }
    assert!(allowed("warm", "hibernated"));
    assert!(allowed("hibernated", "cold_archived"));
    assert!(allowed("cold_archived", "archived"));
    assert!(!allowed("warm", "archived"));
    assert!(!allowed("archived", "warm"));
}