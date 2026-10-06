//! In-process test harness: a real controld against a real Postgres, driven
//! by a fake worker that speaks the actual `/v1/worker` protocol.
//!
//! This exists because the bug that survived longest in this codebase — the
//! question protocol answering a blocked `control_request` with a user
//! message — was invisible to unit tests and only showed up against a live
//! agent. Anything that spans controld, the worker link and the session
//! state machine belongs here rather than in a mocked unit test.
//!
//! Tests are skipped unless `PUKU_TEST_DATABASE_URL` points at a database
//! the suite may **drop and recreate schemas in**. `cargo test` on a laptop
//! with no database still passes; CI sets the variable.

#![cfg(test)]

use std::sync::Arc;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::worker_proto::{Down, Up};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::{api, auth, relay, workerlink, AppState, Config};

/// The 32-byte key credential encryption needs. Fixed for reproducibility;
/// this is test data, not a secret.
const TEST_SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

#[allow(dead_code)] // handles kept for tests that need them, not all do
pub struct Harness {
    pub base: String,
    pub pool: PgPool,
    pub state: AppState,
    pub org: Uuid,
    pub user: Uuid,
    /// Sent as a bearer on every request when set. Only meaningful with
    /// `auth_required`.
    pub key: Option<String>,
    /// The object store, when the harness was started with `snapshots`.
    pub s3: Option<crate::fakes3::FakeS3>,
    /// The fence shared-disk moves go through, when started with
    /// `shared_volumes`.
    pub fence: Option<Arc<RecordingFence>>,
    /// The pool the storage cleanup sees, when started with `shared_volumes`.
    pub pool_admin: Option<Arc<FakePool>>,
}

/// An in-memory Ceph pool for the storage cleanup and disk backups.
#[derive(Default)]
pub struct FakePool {
    pub images: std::sync::Mutex<Vec<String>>,
    /// Images some client has open.
    pub open: std::sync::Mutex<Vec<String>>,
    /// Image contents, and snapshots of them, for backup tests.
    pub data: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    pub snaps: std::sync::Mutex<std::collections::HashMap<(String, String), Vec<u8>>>,
    /// Images that are gone (deleted, or lost with the pool). Every other
    /// name exists, as a booted shared disk would.
    pub lost: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl FakePool {
    /// Create or overwrite an image's contents.
    pub fn write(&self, image: &str, bytes: &[u8]) {
        let mut images = self.images.lock().unwrap();
        if !images.iter().any(|i| i == image) {
            images.push(image.to_string());
        }
        self.data.lock().unwrap().insert(image.to_string(), bytes.to_vec());
        self.lost.lock().unwrap().remove(image);
    }
    /// The image is gone, as after losing the pool.
    pub fn lose(&self, image: &str) {
        self.images.lock().unwrap().retain(|i| i != image);
        self.data.lock().unwrap().remove(image);
        self.snaps.lock().unwrap().retain(|(i, _), _| i != image);
        self.lost.lock().unwrap().insert(image.to_string());
    }
    pub fn read(&self, image: &str) -> Option<Vec<u8>> {
        self.data.lock().unwrap().get(image).cloned()
    }
    pub fn snap_names(&self, image: &str) -> Vec<String> {
        let mut v: Vec<String> =
            self.snaps.lock().unwrap().keys().filter(|(i, _)| i == image).map(|(_, s)| s.clone()).collect();
        v.sort();
        v
    }
}

#[async_trait::async_trait]
impl crate::storagegc::PoolAdmin for FakePool {
    async fn list(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.images.lock().unwrap().clone())
    }
    async fn watchers(&self, image: &str) -> anyhow::Result<Vec<String>> {
        let open = self.open.lock().unwrap().iter().any(|i| i == image);
        Ok(if open { vec!["10.0.0.7:0/9".into()] } else { vec![] })
    }
    async fn remove(&self, image: &str) -> anyhow::Result<()> {
        self.lose(image);
        Ok(())
    }
    async fn exists(&self, image: &str) -> anyhow::Result<bool> {
        Ok(!self.lost.lock().unwrap().contains(image))
    }
    async fn snap_create(&self, image: &str, snap: &str) -> anyhow::Result<()> {
        let data = self.read(image).unwrap_or_default();
        // Like `rbd snap create`: a name that exists is refused, not replaced.
        let mut snaps = self.snaps.lock().unwrap();
        anyhow::ensure!(!snaps.contains_key(&(image.into(), snap.into())), "snapshot {image}@{snap} already exists");
        snaps.insert((image.into(), snap.into()), data);
        Ok(())
    }
    async fn snap_remove(&self, image: &str, snap: &str) -> anyhow::Result<()> {
        self.snaps.lock().unwrap().remove(&(image.to_string(), snap.to_string()));
        Ok(())
    }
    async fn snap_list(&self, image: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.snap_names(image))
    }
    async fn changed_since(&self, image: &str, from: &str, to: &str) -> anyhow::Result<bool> {
        let s = self.snaps.lock().unwrap();
        Ok(s.get(&(image.into(), from.into())) != s.get(&(image.into(), to.into())))
    }
    async fn export_full(&self, image: &str, snap: &str, path: &str) -> anyhow::Result<()> {
        let data = self.snaps.lock().unwrap().get(&(image.into(), snap.into())).cloned().unwrap_or_default();
        std::fs::write(path, data)?;
        Ok(())
    }
    /// A "diff" here is the whole snapshot behind a header naming both ends.
    async fn export_diff(&self, image: &str, from: &str, to: &str, path: &str) -> anyhow::Result<()> {
        let data = self.snaps.lock().unwrap().get(&(image.into(), to.into())).cloned().unwrap_or_default();
        let mut out = format!("DIFF\n{from}\n{to}\n").into_bytes();
        out.extend(data);
        std::fs::write(path, out)?;
        Ok(())
    }
    async fn import_full(&self, path: &str, image: &str, snap: &str) -> anyhow::Result<()> {
        let data = std::fs::read(path)?;
        self.write(image, &data);
        self.snaps.lock().unwrap().insert((image.into(), snap.into()), data);
        Ok(())
    }
    async fn import_diff(&self, path: &str, image: &str) -> anyhow::Result<()> {
        let raw = std::fs::read(path)?;
        let mut parts = raw.splitn(4, |b| *b == b'\n');
        anyhow::ensure!(parts.next() == Some(b"DIFF"), "not a diff");
        let from = String::from_utf8(parts.next().unwrap_or_default().to_vec())?;
        let to = String::from_utf8(parts.next().unwrap_or_default().to_vec())?;
        let data = parts.next().unwrap_or_default().to_vec();
        anyhow::ensure!(
            self.snaps.lock().unwrap().contains_key(&(image.into(), from.clone())),
            "import-diff: start snapshot {from} missing on {image}"
        );
        self.write(image, &data);
        self.snaps.lock().unwrap().insert((image.into(), to), data);
        Ok(())
    }
}

/// A `Fence` that records what it was asked to cut off, and can be told to
/// refuse. Stands in for Ceph: the real blocklist is proven by
/// `puku-volume/tests/real_ceph.rs`.
/// One `fence_volumes` call: old host, session, volumes.
pub type FenceCall = (Uuid, Option<Uuid>, Vec<String>);

#[derive(Default)]
pub struct RecordingFence {
    pub calls: std::sync::Mutex<Vec<FenceCall>>,
    pub refuse: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl puku_fence::Fence for RecordingFence {
    async fn blocklist(&self, _host: Uuid) -> Result<(), puku_fence::FenceError> {
        Ok(())
    }
    async fn fence(
        &self,
        _host: Uuid,
        _bmc: Option<&puku_leases::BmcEndpoint>,
    ) -> Result<puku_fence::FenceReceipt, puku_fence::FenceError> {
        unreachable!("shared-disk moves fence volumes, not whole hosts")
    }
    async fn unfence(&self, _host: Uuid) -> Result<(), puku_fence::FenceError> {
        Ok(())
    }
    async fn fence_volumes(
        &self,
        host_id: Uuid,
        session_id: Option<Uuid>,
        volumes: &[puku_volume::VolumeId],
        _bmc: Option<&puku_leases::BmcEndpoint>,
    ) -> Result<puku_fence::FenceReceipt, puku_fence::FenceError> {
        let vols: Vec<String> = volumes.iter().map(|v| v.to_string()).collect();
        self.calls.lock().unwrap().push((host_id, session_id, vols.clone()));
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(puku_fence::FenceError::Blocklist("refused by the test".into()));
        }
        Ok(puku_fence::FenceReceipt {
            host_id,
            blocklisted_at: chrono::Utc::now(),
            bmc_action: None,
            audit_log_id: 1,
            volumes: vols,
            clients: vec!["10.0.0.9:0/1".into()],
        })
    }
}

#[derive(Default)]
pub struct Opts {
    /// Drop the operator's global model credential. A real deployment
    /// almost always has one; the tests that assert on its absence say so
    /// explicitly rather than inheriting it from a bare harness.
    pub no_operator_key: bool,
    /// Require API keys. With auth off every request runs as the dev
    /// identity **with admin**, which defeats any ownership assertion.
    pub auth_required: bool,
    /// Model a shared deployment: the operator's credentials exist but must
    /// never reach a guest. Distinct from `no_operator_key`, which removes
    /// them -- the interesting case is a key that is present and refused.
    pub multi_tenant: bool,
    /// Object storage (an in-process S3) and machine snapshots on it.
    pub snapshots: bool,
    /// Ceph access for shared session disks, through a `RecordingFence`.
    pub shared_volumes: bool,
}

/// Every test gets its own schema, so they can run concurrently against one
/// database without seeing each other's sessions.
pub async fn start() -> Option<Harness> {
    start_with(Opts::default()).await
}

pub async fn start_with(opts: Opts) -> Option<Harness> {
    let url = std::env::var("PUKU_TEST_DATABASE_URL").ok()?;
    let schema = format!("t{}", Uuid::new_v4().simple());
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect({
            let schema = schema.clone();
            move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {schema}"))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .expect("connecting to PUKU_TEST_DATABASE_URL");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .expect("creating the test schema");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("running migrations");

    let org = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let user = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let s3 = if opts.snapshots { Some(crate::fakes3::FakeS3::start().await) } else { None };
    let blobs = s3.as_ref().map(|(_, endpoint)| {
        let store = crate::blobstore::BlobStore::from_env(
            Some(endpoint),
            Some("puku-cloud"),
            "us-east-1",
            Some("test-access-key"),
            Some("test-secret-key"),
        );
        Arc::new(store.unwrap().unwrap())
    });
    let fence = opts.shared_volumes.then(|| Arc::new(RecordingFence::default()));
    let pool_admin = opts.shared_volumes.then(|| Arc::new(FakePool::default()));
    let shared_volumes = fence.clone().zip(pool_admin.clone()).map(|(f, admin)| {
        Arc::new(crate::sharedvol::SharedVolumes {
            pool: "puku-sessions".into(),
            fence: f,
            admin,
            gc: crate::storagegc::GcPolicy { grace: std::time::Duration::ZERO, ..Default::default() },
            backup: Some(crate::diskbackup::BackupPolicy {
                tmp_dir: std::env::temp_dir().join(format!("puku-bk-{}", Uuid::new_v4().simple())),
                ..Default::default()
            }),
        })
    });
    let state = AppState {
        // The integration harness runs without a memory service: memory is
        // additive capability, and every call site must cope with its absence.
        memory: None,
        pool: pool.clone(),
        connectors: None,
        skills: None,
        platform: None,
        secrets: crate::secretbox::SecretBox::from_env(Some(TEST_SECRET))
            .unwrap()
            .map(Arc::new),
        blobs,
        hub: relay::SessionHub::new(),
        workers: workerlink::WorkerRegistry::new(),
        github: Arc::new(None),
        cfg: Arc::new(Config {
            memory_preamble_bytes: 4096,
            worker_token: "test-worker-token".into(),
            allow_shared_worker_token: true,
            agent_image: "test-image".into(),
            puku_api_key: (!opts.no_operator_key).then(|| "operator-fallback-key".to_string()),
            puku_oauth_token: None,
            puku_session_file: None,
            git_token: Some("operator-pat".into()),
            // Most tests only need a session to reach a worker, and an
            // operator key is the cheapest way to give them one. It tracks
            // `no_operator_key`, so a test that opts out gets a genuinely
            // credential-less deployment -- which is what the multi-tenant
            // refusal is asserted against.
            allow_operator_credentials: !opts.no_operator_key && !opts.multi_tenant,
            // No fleet-wide tool policy: a test that cares about tool lists
            // sets them on the request, and the rest must not inherit a
            // restriction nobody asked for.
            default_disallowed_tools: Vec::new(),
            default_allowed_tools: Vec::new(),
            auth_issuer: "http://127.0.0.1:1/oauth".into(),
            api_url: "http://127.0.0.1:1".into(),
            auth_required: opts.auth_required,
            permission_ceiling: puku_cloud_proto::session::PermissionMode::BypassPermissions,
            default_max_turns: 50,
            instance_id: Uuid::new_v4(),
            dev_org: org,
            dev_user: user,
            engine_default: puku_cloud_proto::Engine::Libkrun,
            // Both offered, so a test can ask for Cloud Hypervisor and prove
            // where it is -- and is not -- placed.
            engines_allowed: vec![
                puku_cloud_proto::Engine::Libkrun,
                puku_cloud_proto::Engine::CloudHypervisor,
            ],
            machine_image: "test-computer".into(),
            machine_max_cpus: 4,
            machine_max_memory_mib: 8192,
            links_base: "http://links.test".into(),
            machine_idle_sweep_s: 30,
            // The sweep never runs on its own here: tests call it.
            snapshots: opts.snapshots.then_some(crate::snapshots::SnapshotConfig {
                part_bytes: 5 << 20,
                keep: 5,
                retain_destroyed_days: 7,
                sweep_s: 3600,
            }),
        }),
        data: crate::datalink::DataPool::new(),
        links: Arc::new(crate::links::LinkSigner::new(b"test-links")),
        shared_volumes,
    };

    // Port 0: the OS picks a free one, so tests never collide.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = api::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let key = if opts.auth_required {
        Some(issue_key(&pool, org, Some(user), false).await)
    } else {
        None
    };

    Some(Harness {
        base: format!("http://{addr}"),
        pool,
        state,
        org,
        user,
        key,
        s3: s3.map(|(store, _)| store),
        fence,
        pool_admin,
    })
}

impl Harness {
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let res = self
            .auth(reqwest::Client::new().post(format!("{}{path}", self.base)))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    pub async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let res = self
            .auth(reqwest::Client::new().get(format!("{}{path}", self.base)))
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// Poll until a session reaches one of `states`, or give up. Sessions
    /// move through the state machine asynchronously, so a fixed sleep
    /// would make these tests flaky on a loaded machine.
    /// Poll until the session reaches one of `states`.
    ///
    /// The budget is 15s, not 5s, because two things stack under a full
    /// concurrent suite: `dispatch_pending` only ticks every 3 seconds, and
    /// ~100 tests share one Postgres. A resume needs a dispatch tick it may
    /// have just missed, so 5s was close enough to the floor that the resume
    /// tests failed in the suite while passing alone. A passing test returns
    /// on the first match and waits no longer for this.
    pub async fn await_state(&self, id: Uuid, states: &[&str]) -> String {
        for _ in 0..300 {
            let (_, s) = self.get(&format!("/v1/sessions/{id}")).await;
            if let Some(st) = s.get("state").and_then(|v| v.as_str()) {
                if states.contains(&st) {
                    return st.to_string();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("session {id} never reached any of {states:?}");
    }
}

/// A worker connection that speaks the real protocol over a real socket.
pub struct FakeWorker {
    tx: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    rx: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
}

impl FakeWorker {
    /// A worker exactly as the previous release registered: no engines
    /// named, which controld must read as libkrun-only.
    pub async fn connect(h: &Harness, name: &str, token: &str) -> Result<Self> {
        Self::connect_with_engines(h, name, token, vec![]).await
    }

    pub async fn connect_with_engines(
        h: &Harness,
        name: &str,
        token: &str,
        engines: Vec<puku_cloud_proto::Engine>,
    ) -> Result<Self> {
        let url = format!("{}/v1/worker", h.base.replace("http://", "ws://"));
        let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
        let (tx, rx) = ws.split();
        let mut w = FakeWorker { tx, rx };
        w.send(Up::Register {
            worker_name: name.into(),
            auth_token: token.into(),
            capacity_slots: 4,
            msb_version: "test".into(),
            running_sessions: vec![],
            on_disk_sessions: vec![],
            engines,
            features: vec![],
            running_machines: vec![],
            on_disk_machines: vec![],
            host: None,
        })
        .await?;
        Ok(w)
    }

    /// A worker coming back after a crash or partition, still running
    /// `running_sessions`. Returns once registered.
    pub async fn connect_returning(h: &Harness, name: &str, running_sessions: Vec<Uuid>) -> Result<Self> {
        let url = format!("{}/v1/worker", h.base.replace("http://", "ws://"));
        let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
        let (tx, rx) = ws.split();
        let mut w = FakeWorker { tx, rx };
        w.send(Up::Register {
            worker_name: name.into(),
            auth_token: "test-worker-token".into(),
            capacity_slots: 4,
            msb_version: "test".into(),
            on_disk_sessions: running_sessions.clone(),
            running_sessions,
            engines: vec![],
            features: vec![],
            running_machines: vec![],
            on_disk_machines: vec![],
            host: None,
        })
        .await?;
        Ok(w)
    }

    pub async fn send(&mut self, frame: Up) -> Result<()> {
        self.tx
            .send(Message::Text(serde_json::to_string(&frame)?.into()))
            .await?;
        Ok(())
    }

    /// Next frame from controld, or None if the link closed (which is how a
    /// rejected registration presents).
    ///
    /// The timeout must clear a `dispatch_pending` tick with room to spare.
    /// At 5s it did not: a resumed session waits for the next 3s tick, and
    /// under a full concurrent suite sharing one Postgres that regularly
    /// crossed the line. The timeout returns None, `next_assignment` passes
    /// it straight through, and the resume tests failed on
    /// `expect("reassigned on resume")` while passing in isolation.
    pub async fn recv(&mut self) -> Option<Down> {
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(15), self.rx.next())
                .await
                .ok()??;
            match msg.ok()? {
                Message::Text(t) => return serde_json::from_str(&t).ok(),
                Message::Close(_) => return None,
                _ => continue,
            }
        }
    }

    /// Wait for the next `AssignSession`, ignoring acks and other frames.
    pub async fn next_assignment(&mut self) -> Option<puku_cloud_proto::session::SessionSpec> {
        for _ in 0..20 {
            match self.recv().await? {
                Down::AssignSession { spec } => return Some(spec),
                _ => continue,
            }
        }
        None
    }

    /// Assert no session is assigned within `window`.
    ///
    /// Absence can only be proved by waiting, but it does not need the full
    /// `recv` timeout: callers use this *after* the state machine has already
    /// settled, so anything that was going to be dispatched has been. Waiting
    /// out `recv`'s own timeout instead added its entire duration to the
    /// suite for one assertion.
    pub async fn assert_no_assignment(&mut self, window: std::time::Duration) {
        if let Ok(Some(spec)) = tokio::time::timeout(window, self.next_assignment()).await {
            panic!("a session was assigned when none should have been: {}", spec.session_id);
        }
    }

    /// Wait for the answer to `Up::RequestGitToken`.
    ///
    /// Returns the frame's `token`, so `Some(None)` (a reply carrying no
    /// token) is distinguishable from `None` (no reply at all) -- the
    /// difference between "refused" and "never answered".
    pub async fn next_git_token(&mut self) -> Option<Option<String>> {
        for _ in 0..20 {
            match self.recv().await? {
                Down::GitToken { token, .. } => return Some(token),
                _ => continue,
            }
        }
        None
    }

    /// A worker that runs machines, on every engine, reporting no host
    /// limits -- which placement must read as "never too large".
    pub async fn connect_machine_worker(h: &Harness, name: &str) -> Result<Self> {
        Self::connect_machine_worker_with(h, name, 8, None).await
    }

    /// A machines worker with a given capacity and host report.
    ///
    /// Returns once controld has registered it. Machine creates fail fast,
    /// so one sent before the worker is in the registry is refused for want
    /// of a worker -- correct, but not what a test that just connected one
    /// means to check.
    pub async fn connect_machine_worker_with(
        h: &Harness,
        name: &str,
        capacity_slots: u32,
        host: Option<puku_cloud_proto::worker_proto::HostReport>,
    ) -> Result<Self> {
        let features = vec![puku_cloud_proto::worker_proto::FEATURE_MACHINES.to_string()];
        Self::connect_worker_with(h, name, capacity_slots, host, features).await
    }

    /// A machines worker that keeps a liveness lease (`FEATURE_LEASE`).
    /// It renews only when the test sends `Up::LeaseRenew`.
    pub async fn connect_lease_worker(h: &Harness, name: &str) -> Result<Self> {
        let features = vec![
            puku_cloud_proto::worker_proto::FEATURE_MACHINES.to_string(),
            puku_cloud_proto::worker_proto::FEATURE_LEASE.to_string(),
        ];
        Self::connect_worker_with(h, name, 8, None, features).await
    }

    /// A worker whose session disks are on the shared Ceph cluster.
    pub async fn connect_shared_worker(h: &Harness, name: &str) -> Result<Self> {
        let features = vec![
            puku_cloud_proto::worker_proto::FEATURE_LEASE.to_string(),
            puku_cloud_proto::worker_proto::FEATURE_SHARED_VOLUMES.to_string(),
        ];
        Self::connect_worker_with(h, name, 8, None, features).await
    }

    /// A machines worker whose machine disks are on the shared cluster.
    pub async fn connect_shared_machine_worker(h: &Harness, name: &str) -> Result<Self> {
        let features = vec![
            puku_cloud_proto::worker_proto::FEATURE_MACHINES.to_string(),
            puku_cloud_proto::worker_proto::FEATURE_LEASE.to_string(),
            puku_cloud_proto::worker_proto::FEATURE_SHARED_VOLUMES.to_string(),
        ];
        Self::connect_worker_with(h, name, 8, None, features).await
    }

    /// A worker that runs machines and snapshots them.
    pub async fn connect_snapshot_worker(h: &Harness, name: &str) -> Result<Self> {
        let features = vec![
            puku_cloud_proto::worker_proto::FEATURE_MACHINES.to_string(),
            puku_cloud_proto::snapshot::FEATURE_SNAPSHOTS.to_string(),
        ];
        Self::connect_worker_with(h, name, 8, None, features).await
    }

    async fn connect_worker_with(
        h: &Harness,
        name: &str,
        capacity_slots: u32,
        host: Option<puku_cloud_proto::worker_proto::HostReport>,
        features: Vec<String>,
    ) -> Result<Self> {
        let url = format!("{}/v1/worker", h.base.replace("http://", "ws://"));
        let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
        let (tx, rx) = ws.split();
        let mut w = FakeWorker { tx, rx };
        w.send(Up::Register {
            worker_name: name.into(),
            auth_token: "test-worker-token".into(),
            capacity_slots,
            msb_version: "test".into(),
            running_sessions: vec![],
            on_disk_sessions: vec![],
            engines: vec![puku_cloud_proto::Engine::Libkrun, puku_cloud_proto::Engine::CloudHypervisor],
            features,
            running_machines: vec![],
            on_disk_machines: vec![],
            host,
        })
        .await?;
        loop {
            match w.recv().await {
                Some(Down::RegisterAck { .. }) => return Ok(w),
                Some(_) => continue,
                None => anyhow::bail!("controld refused the registration"),
            }
        }
    }

    /// The next frame matching `pick`, skipping everything else.
    pub async fn next_matching<T>(&mut self, mut pick: impl FnMut(Down) -> Option<T>) -> Option<T> {
        for _ in 0..40 {
            if let Some(t) = pick(self.recv().await?) {
                return Some(t);
            }
        }
        None
    }

    pub async fn next_machine_assignment(&mut self) -> Option<puku_cloud_proto::machine::MachineSpec> {
        self.next_matching(|d| match d {
            Down::AssignMachine { spec } => Some(spec),
            _ => None,
        })
        .await
    }

    /// Report a machine's boot as a real worker would.
    pub async fn machine_state(
        &mut self,
        spec: &puku_cloud_proto::machine::MachineSpec,
        state: puku_cloud_proto::machine::MachineState,
        volume_existed: bool,
    ) {
        self.send(Up::MachineState {
            machine_id: spec.machine_id,
            generation: spec.generation,
            state,
            error: None,
            volume_existed,
            reason: None,
        })
        .await
        .unwrap();
    }

    /// Carry out a snapshot order as a real worker would: upload `bytes` as
    /// every layer through the URLs controld presigns, then report it ready.
    pub async fn upload_snapshot(&mut self, order: &puku_cloud_proto::snapshot::SnapshotOrder, bytes: &[u8]) {
        use puku_cloud_proto::snapshot::{Consistency, Fingerprint, PartDone, SnapshotProgress};
        for layer in &order.layers {
            let wanted = layer.layer;
            self.send(Up::RequestSnapshotUrls { snapshot_id: order.snapshot_id, layer: wanted, first_part: 1, count: 1 })
                .await
                .unwrap();
            let (urls, error) = self
                .next_matching(|d| match d {
                    Down::SnapshotUrls { snapshot_id, layer, urls, error, .. }
                        if snapshot_id == order.snapshot_id && layer == wanted =>
                    {
                        Some((urls, error))
                    }
                    _ => None,
                })
                .await
                .expect("controld answers with part URLs");
            assert!(error.is_none(), "part URLs refused: {error:?}");
            let resp = reqwest::Client::new().put(&urls[0].url).body(bytes.to_vec()).send().await.unwrap();
            assert!(resp.status().is_success(), "part upload: {}", resp.status());
            let etag = resp.headers()["etag"].to_str().unwrap().to_string();
            self.send(Up::SnapshotLayerDone {
                snapshot_id: order.snapshot_id,
                layer: wanted,
                parts: vec![PartDone { part: 1, etag, bytes: bytes.len() as u64 }],
                plain_bytes: bytes.len() as u64 * 2,
                stored_bytes: bytes.len() as u64,
                sha256: "ab".repeat(32),
            })
            .await
            .unwrap();
        }
        self.send(Up::SnapshotState {
            snapshot_id: order.snapshot_id,
            machine_id: order.machine_id,
            state: SnapshotProgress::Ready,
            consistency: Some(Consistency::Clean),
            error: None,
            fingerprint: Some(Fingerprint { volume: Some(format!("fp-{}", order.snapshot_id)), root: None }),
            reused: vec![],
        })
        .await
        .unwrap();
    }

    /// Report a boot this worker could not complete, as a real one would.
    pub async fn machine_failed(&mut self, spec: &puku_cloud_proto::machine::MachineSpec, reason: &str, error: &str) {
        self.send(Up::MachineState {
            machine_id: spec.machine_id,
            generation: spec.generation,
            state: puku_cloud_proto::machine::MachineState::Failed,
            error: Some(error.into()),
            volume_existed: false,
            reason: Some(reason.into()),
        })
        .await
        .unwrap();
    }

    /// Wait for the next input line delivered to the guest's stdin.
    pub async fn next_input(&mut self) -> Option<String> {
        for _ in 0..20 {
            match self.recv().await? {
                Down::DeliverInput { stream_json_line, .. } => return Some(stream_json_line),
                _ => continue,
            }
        }
        None
    }
}

type ClientWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One idle data socket, offered to controld the way workerd's pool does.
pub struct FakeDataSocket {
    ws: ClientWs,
}

impl FakeDataSocket {
    pub async fn offer(h: &Harness, worker_name: &str) -> Result<Self> {
        Self::offer_with_token(h, worker_name, "test-worker-token").await
    }

    pub async fn offer_with_token(h: &Harness, worker_name: &str, token: &str) -> Result<Self> {
        let url = format!("{}/v1/worker/data", h.base.replace("http://", "ws://"));
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;
        let hello = puku_cloud_proto::data_proto::DataHello {
            worker_name: worker_name.into(),
            auth_token: token.into(),
        };
        ws.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
        Ok(FakeDataSocket { ws })
    }

    /// Whether controld hangs up on this socket within `secs`, rather than parking it.
    pub async fn closed_within(&mut self, secs: u64) -> bool {
        match tokio::time::timeout(std::time::Duration::from_secs(secs), self.ws.next()).await {
            // Still open: parked in the pool.
            Err(_) => false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => true,
            // A frame, e.g. a stream header: it was put to use.
            Ok(Some(Ok(_))) => false,
        }
    }

    /// Wait for controld to put this socket to use.
    pub async fn header(&mut self) -> puku_cloud_proto::data_proto::StreamHeader {
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(15), self.ws.next())
                .await
                .expect("a stream header in time")
                .expect("an open socket")
                .unwrap();
            if let Message::Text(t) = msg {
                return serde_json::from_str(&t).expect("a stream header");
            }
        }
    }

    pub async fn msg(&mut self, m: &puku_cloud_proto::data_proto::DataMsg) {
        self.ws.send(Message::Text(serde_json::to_string(m).unwrap().into())).await.unwrap();
    }

    pub async fn bytes(&mut self, b: &[u8]) {
        self.ws.send(Message::Binary(b.to_vec().into())).await.unwrap();
    }

    /// Binary payload up to the next `Eof`.
    pub async fn read_to_eof(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(Ok(msg)) = self.ws.next().await {
            match msg {
                Message::Binary(b) => out.extend_from_slice(&b),
                Message::Text(t)
                    if matches!(
                        serde_json::from_str(&t),
                        Ok(puku_cloud_proto::data_proto::DataMsg::Eof)
                    ) =>
                {
                    break
                }
                _ => {}
            }
        }
        out
    }

    /// Binary payload until `needle` has been seen (an HTTP request head).
    pub async fn read_until(&mut self, needle: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while !out.windows(needle.len()).any(|w| w == needle) {
            match tokio::time::timeout(std::time::Duration::from_secs(10), self.ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => out.extend_from_slice(&b),
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        out
    }
}

/// Mint an api key directly, bypassing the CLI subcommand.
///
/// `user` matters: a key with no user is an *org* key and sees every
/// session in the org by design, so any per-user ownership assertion needs
/// a key bound to someone.
pub async fn issue_key(pool: &PgPool, org: Uuid, user: Option<Uuid>, admin: bool) -> String {
    let key = auth::generate_key();
    let scopes: Vec<String> = if admin { vec!["admin".into()] } else { vec![] };
    sqlx::query(
        "INSERT INTO api_keys (id, org_id, user_id, key_hash, prefix, scopes) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(Uuid::new_v4())
    .bind(org)
    .bind(user)
    .bind(auth::hash_key(&key))
    .bind(&key[..12])
    .bind(&scopes)
    .execute(pool)
    .await
    .unwrap();
    key
}
