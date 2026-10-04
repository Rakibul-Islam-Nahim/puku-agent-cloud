//! `/v1/machines`: generic VMs driven from outside. Contract:
//! docs/MACHINES-API.md.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use hyper_util::rt::TokioIo;
use puku_cloud_proto::data_proto::{DataMsg, StreamTarget};
use puku_cloud_proto::machine::{image_key, slots_for, Entrypoint, MachineSpec, MachineState, VolumeSpec};
use puku_cloud_proto::snapshot::FEATURE_SNAPSHOTS;
use puku_cloud_proto::worker_proto::{Down, FEATURE_MACHINES};
use puku_cloud_proto::Engine;
use serde::Deserialize;
use uuid::Uuid;

use super::{owns, resolve_engine, ApiResult, AppError};
use crate::auth::AuthCtx;
use crate::datalink::{self, StreamError};
use crate::db::machines::{self as mdb, MachineFields, MachineRow};
use crate::db::snapshots::{self as sdb, SnapshotRow};
use crate::snapshots::SnapshotPolicy;
use crate::workerlink::{Placement, Unplaceable};
use puku_cloud_proto::snapshot::SnapshotTrigger;
use crate::AppState;

/// Default and ceiling for how long a request may block on a state change.
const MAX_WAIT_S: u64 = 300;
const DEFAULT_EXEC_TIMEOUT_MS: u64 = 300_000;
const MAX_EXEC_TIMEOUT_MS: u64 = 3_600_000;
/// Same grace a session gets before its volume host is given up on.
const VOLUME_HOST_GRACE: chrono::Duration = chrono::Duration::minutes(15);
/// Free disk a worker needs before a machine is placed there at all. Below
/// this the boot fails anyway, later and less clearly.
const MIN_FREE_DISK_MIB: u64 = 1024;
/// What a worker must run to boot a machine, and to boot one from a snapshot.
const MACHINE_FEATURES: &[&str] = &[FEATURE_MACHINES];
const RESTORE_FEATURES: &[&str] = &[FEATURE_MACHINES, FEATURE_SNAPSHOTS];

/// Routes behind the api-key middleware.
pub fn protected() -> Router<AppState> {
    Router::new()
        .route("/v1/machines", post(create_machine).get(list_machines))
        .route("/v1/machines/{id}", get(get_machine).delete(delete_machine))
        .route("/v1/machines/{id}/start", post(start_machine))
        .route("/v1/machines/{id}/stop", post(stop_machine))
        .route("/v1/machines/{id}/touch", post(touch_machine))
        .route("/v1/machines/{id}/restore", post(restore_machine))
        .route("/v1/machines/{id}/snapshots", post(create_snapshot).get(list_snapshots))
        .route("/v1/machines/{id}/snapshots/{sid}", get(get_snapshot).delete(delete_snapshot))
        .route("/v1/machines/{id}/exec", post(exec))
        .route(
            "/v1/machines/{id}/files",
            get(read_files).put(write_file).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/machines/{id}/archive",
            get(get_archive).put(put_archive).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/v1/machines/{id}/links", post(create_link))
        .route("/v1/machines/{id}/ports/{port}", any(port_proxy_root))
        // Same handler for the trailing-slash form, which the {*rest} route
        // refuses to match against an empty rest. Without this the noVNC
        // landing URL `/ports/6080/` 404s even though `/ports/6080` 200s.
        .route("/v1/machines/{id}/ports/{port}/", any(port_proxy_root))
        .route("/v1/machines/{id}/ports/{port}/{*rest}", any(port_proxy))
}

/// Routes that carry their own credential: a capability in the path, or the
/// worker token inside the first frame.
pub fn public() -> Router<AppState> {
    Router::new()
        .route("/v1/links/{cap}", any(link_proxy_root))
        // As for /ports/{port}/: {*rest} will not match an empty rest.
        .route("/v1/links/{cap}/", any(link_proxy_root))
        .route("/v1/links/{cap}/{*rest}", any(link_proxy))
        .route("/v1/worker/data", get(data_ws))
}

fn not_found(id: Uuid) -> AppError {
    AppError(StatusCode::NOT_FOUND, format!("machine {id} not found"))
}

fn bad_request(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

impl From<StreamError> for AppError {
    fn from(e: StreamError) -> Self {
        AppError(StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY), e.message)
    }
}

async fn load_owned(state: &AppState, ctx: &AuthCtx, id: Uuid) -> ApiResult<MachineRow> {
    let row = mdb::get(&state.pool, id).await?.ok_or_else(|| not_found(id))?;
    if !owns(ctx, row.org_id, row.user_id) {
        return Err(not_found(id));
    }
    Ok(row)
}

// ------------------------------------------------------------- refusals

/// A lifecycle request the fleet cannot serve, with a reason a client can
/// act on: retry later, fix the deployment, or move the machine.
///
/// Returned at once. Waiting out `wait_s` in `scheduled` only ever turned
/// "no worker runs this" into a two-minute hang that explained nothing.
struct Refused {
    status: StatusCode,
    reason: &'static str,
    message: String,
    detail: serde_json::Value,
    retry_after_s: Option<u64>,
    machine: Option<serde_json::Value>,
}

impl Refused {
    fn new(status: StatusCode, reason: &'static str, message: impl Into<String>) -> Self {
        Refused {
            status,
            reason,
            message: message.into(),
            detail: serde_json::json!({}),
            retry_after_s: None,
            machine: None,
        }
    }

    /// An existing error, now with a reason.
    fn plain(e: AppError, reason: &'static str) -> Self {
        Refused::new(e.0, reason, e.1)
    }

    fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }

    fn retry_after(mut self, seconds: Option<u64>) -> Self {
        self.retry_after_s = seconds;
        self
    }

    fn with_machine(mut self, m: &MachineRow) -> Self {
        self.machine = Some(m.to_api());
        self
    }
}

/// What the lifecycle routes fail with.
enum LifecycleError {
    Plain(AppError),
    Refused(Box<Refused>),
}

type LifeResult<T> = Result<T, LifecycleError>;

impl From<AppError> for LifecycleError {
    fn from(e: AppError) -> Self {
        LifecycleError::Plain(e)
    }
}

impl From<anyhow::Error> for LifecycleError {
    fn from(e: anyhow::Error) -> Self {
        LifecycleError::Plain(e.into())
    }
}

impl From<Refused> for LifecycleError {
    fn from(r: Refused) -> Self {
        LifecycleError::Refused(Box::new(r))
    }
}

impl IntoResponse for LifecycleError {
    fn into_response(self) -> Response {
        let r = match self {
            LifecycleError::Plain(e) => return e.into_response(),
            LifecycleError::Refused(r) => r,
        };
        // Info, not error: this is the fleet's state, not a bug here, and a
        // client retrying against a full fleet would fill the error log.
        tracing::info!(status = r.status.as_u16(), reason = r.reason, message = %r.message, "machine request refused");
        // The same `error.message` every other failure has, so a client that
        // reads only that keeps working; `reason` and the rest ride along.
        let mut error = serde_json::json!({"message": r.message, "reason": r.reason, "detail": r.detail});
        if let Some(s) = r.retry_after_s {
            error["retry_after_s"] = s.into();
        }
        let mut body = serde_json::json!({"error": error});
        if let Some(m) = r.machine {
            body["machine"] = m;
        }
        let mut resp = (r.status, Json(body)).into_response();
        if let Some(s) = r.retry_after_s {
            resp.headers_mut().insert(axum::http::header::RETRY_AFTER, HeaderValue::from(s));
        }
        resp
    }
}

/// The HTTP form of an [`Unplaceable`], with what a client needs to act on.
async fn refusal(state: &AppState, why: Unplaceable, image: &str) -> Refused {
    use serde_json::json;
    let reason = why.reason();
    let unavailable = StatusCode::SERVICE_UNAVAILABLE;
    let unfit = StatusCode::UNPROCESSABLE_ENTITY;
    let (status, retry, message, detail) = match why {
        Unplaceable::NoWorkers => {
            (unavailable, Some(30), "no worker is connected to the control plane".to_string(), json!({}))
        }
        Unplaceable::EngineUnavailable { engine } => (
            unavailable,
            Some(60),
            format!("no connected worker runs the {engine} engine"),
            json!({"engine": engine}),
        ),
        Unplaceable::MissingFeature { feature } => (
            unfit,
            None,
            format!("no connected worker supports {feature}; its workerd needs upgrading"),
            json!({"feature": feature}),
        ),
        Unplaceable::AllDraining => {
            (unavailable, Some(60), "every worker that could run this machine is draining".to_string(), json!({}))
        }
        Unplaceable::TooLarge { requested_slots, cpus, max_slots, max_cores } => {
            let message = match (max_cores, max_slots) {
                (Some(cores), _) if cpus > cores => {
                    format!("this machine asks for {cpus} vCPUs; the largest connected worker has {cores} cores")
                }
                (_, Some(max)) => {
                    format!("this machine needs {requested_slots} slots; the largest connected worker holds {max}")
                }
                _ => "this machine is larger than any connected worker can hold".to_string(),
            };
            let detail = json!({
                "requested_slots": requested_slots, "cpus": cpus,
                "max_slots": max_slots, "max_cores": max_cores,
            });
            (unfit, None, message, detail)
        }
        Unplaceable::ImageNotStaged { image_key } => (
            unfit,
            None,
            format!(
                "image {image} is not staged for cloud_hypervisor on any worker that could run this \
                 machine; run deploy/scripts/build-ch-rootfs.sh {image} on one"
            ),
            json!({"image": image, "key": image_key}),
        ),
        Unplaceable::InsufficientDisk { needed_mib, best_free_mib } => (
            unavailable,
            Some(60),
            format!("no worker has {needed_mib} MiB of disk free"),
            json!({"needed_mib": needed_mib, "best_free_mib": best_free_mib}),
        ),
        Unplaceable::CapacityFull { requested_slots, best_free_slots, workers } => (
            unavailable,
            // Capacity frees on a worker heartbeat, every 10 s.
            Some(15),
            format!("no worker has {requested_slots} free slots right now (the most free is {best_free_slots})"),
            json!({"requested_slots": requested_slots, "best_free_slots": best_free_slots, "workers": workers}),
        ),
        Unplaceable::VolumeHostOffline { worker_id } => {
            let presence = crate::db::worker_presence(&state.pool, worker_id).await.ok().flatten();
            let name = presence.as_ref().map(|p| p.0.clone());
            let offline_for = presence.and_then(|p| p.2).map(|hb| (chrono::Utc::now() - hb).num_seconds().max(0));
            let grace_left = offline_for.map_or(0, |o| (VOLUME_HOST_GRACE.num_seconds() - o).max(0));
            (
                unavailable,
                Some(grace_left.clamp(5, 60) as u64),
                format!(
                    "the worker holding this machine's volume ({}) is offline; if it is not back within \
                     {grace_left}s the machine starts elsewhere with an empty volume",
                    name.as_deref().unwrap_or("unknown")
                ),
                json!({
                    "worker_id": worker_id, "worker": name,
                    "offline_for_s": offline_for, "grace_left_s": grace_left,
                }),
            )
        }
        Unplaceable::VolumeHostFull { worker_id, requested_slots, free_slots } => (
            unavailable,
            Some(15),
            format!("the worker holding this machine's volume has {free_slots} free slots; it needs {requested_slots}"),
            json!({"worker_id": worker_id, "requested_slots": requested_slots, "free_slots": free_slots}),
        ),
    };
    Refused::new(status, reason, message).detail(detail).retry_after(retry)
}

// -------------------------------------------------------------- lifecycle

#[derive(Deserialize, Default)]
struct CreateMachineReq {
    external_id: Option<String>,
    image: Option<String>,
    engine: Option<Engine>,
    cpus: Option<i32>,
    memory_mib: Option<i32>,
    #[serde(default)]
    expose: Vec<i64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    secret_env: BTreeMap<String, String>,
    entrypoint: Option<Entrypoint>,
    volume: Option<VolumeSpec>,
    idle_timeout_s: Option<i64>,
    max_duration_s: Option<i64>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    wait_s: Option<u64>,
    /// Keep a machine nothing can place right now scheduled instead of
    /// refusing it (see `StartOpts::queue`).
    #[serde(default)]
    queue: bool,
    /// See `StartOpts::relocate`.
    #[serde(default)]
    relocate: bool,
    /// Keep the root disk across stop/start (Cloud Hypervisor only).
    #[serde(default)]
    persist_root: bool,
    snapshots: Option<crate::snapshots::SnapshotPolicy>,
}

/// An absolute guest path with no `..` in it.
fn guest_path(p: &str) -> ApiResult<()> {
    if !p.starts_with('/') || p.contains('\0') || p.split('/').any(|seg| seg == "..") {
        return Err(bad_request(format!("{p:?} is not an absolute path without `..`")));
    }
    Ok(())
}

fn fields_from(state: &AppState, req: &CreateMachineReq, engine: Engine) -> ApiResult<MachineFields> {
    let mut expose = Vec::new();
    for p in &req.expose {
        if !(1..=65535).contains(p) {
            return Err(bad_request(format!("port {p} is out of range")));
        }
        if !expose.contains(&(*p as i32)) {
            expose.push(*p as i32);
        }
    }
    if let Some(ep) = &req.entrypoint {
        if ep.argv.is_empty() {
            return Err(bad_request("entrypoint.argv must name a command"));
        }
    }
    if let Some(v) = &req.volume {
        guest_path(&v.path)?;
        if v.path == "/" {
            return Err(bad_request("the volume cannot be mounted at /"));
        }
    }
    if req.external_id.as_ref().is_some_and(|e| e.is_empty() || e.len() > 200) {
        return Err(bad_request("external_id must be 1-200 characters"));
    }
    for k in req.env.keys().chain(req.secret_env.keys()) {
        if k.is_empty() || k.contains('=') || k.contains('\0') {
            return Err(bad_request(format!("{k:?} is not an environment variable name")));
        }
    }
    if req.persist_root && engine != Engine::CloudHypervisor {
        return Err(bad_request(format!(
            "persist_root needs the cloud_hypervisor engine; {engine} keeps no root disk across stops"
        )));
    }
    let policy = req.snapshots.clone().unwrap_or_default();
    policy.validate().map_err(bad_request)?;
    let secret_env_enc = if req.secret_env.is_empty() {
        None
    } else {
        let secrets = state.secrets.as_ref().ok_or_else(|| {
            bad_request("secret_env needs PUKU_SECRET_KEY configured on the control plane")
        })?;
        Some(secrets.encrypt(&serde_json::to_string(&req.secret_env).unwrap())?)
    };
    let cfg = &state.cfg;
    Ok(MachineFields {
        engine,
        image: req.image.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| cfg.machine_image.clone()),
        cpus: req.cpus.unwrap_or(2).clamp(1, cfg.machine_max_cpus as i32),
        memory_mib: req.memory_mib.unwrap_or(2048).clamp(512, cfg.machine_max_memory_mib as i32),
        expose,
        env: serde_json::to_value(&req.env).unwrap(),
        secret_env_enc,
        entrypoint: req.entrypoint.as_ref().map(|e| serde_json::to_value(e).unwrap()),
        volume: req.volume.as_ref().map(|v| serde_json::to_value(v).unwrap()),
        labels: serde_json::to_value(&req.labels).unwrap(),
        idle_timeout_s: req.idle_timeout_s.unwrap_or(0).clamp(0, i32::MAX as i64) as i32,
        max_duration_s: req.max_duration_s.unwrap_or(0).clamp(0, i32::MAX as i64) as i32,
        persist_root: req.persist_root,
        snapshot_policy: serde_json::to_value(&policy).unwrap(),
    })
}

async fn create_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<CreateMachineReq>,
) -> LifeResult<Json<serde_json::Value>> {
    let engine = resolve_engine(&state, req.engine).map_err(|e| Refused::plain(e, "engine_not_allowed"))?;
    let fields = fields_from(&state, &req, engine)?;
    let opts = StartOpts { wait: req.wait_s.unwrap_or(0).min(MAX_WAIT_S), queue: req.queue, relocate: req.relocate };

    if let Some(ext) = req.external_id.as_deref() {
        if let Some(existing) = mdb::get_by_external(&state.pool, ctx.org_id, ext).await? {
            return ensure_existing(&state, &ctx, existing, &fields, opts).await;
        }
    }
    let (live, cap) = mdb::quota(&state.pool, ctx.org_id).await?;
    if live >= cap {
        return Err(Refused::new(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            format!("machine quota reached ({live}/{cap})"),
        )
        .detail(serde_json::json!({"machines": live, "max_concurrent_machines": cap}))
        .into());
    }
    // Refuse before creating anything: a machine no worker can run should
    // hold neither a quota slot nor its external_id.
    if !opts.queue {
        let placement = placement_for(engine, fields.cpus, fields.memory_mib, &fields.image, None, false);
        if let Err(why) = state.workers.diagnose(&placement) {
            return Err(refusal(&state, why, &fields.image).await.into());
        }
    }
    let row = match mdb::insert(&state.pool, ctx.org_id, ctx.user_id, req.external_id.as_deref(), &fields).await {
        Ok(row) => row,
        // Two creates for one external_id raced; the loser ensures the
        // winner's machine instead of failing.
        Err(e) if is_unique_violation(&e) => {
            let ext = req.external_id.as_deref().unwrap_or_default();
            let existing = mdb::get_by_external(&state.pool, ctx.org_id, ext)
                .await?
                .ok_or_else(|| AppError::from(e))?;
            return ensure_existing(&state, &ctx, existing, &fields, opts).await;
        }
        Err(e) => return Err(e.into()),
    };
    crate::db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "machine.create", &row.id.to_string(), serde_json::json!({"engine": row.engine}))
        .await
        .ok();
    place_and_wait(&state, row, false, opts).await
}

fn is_unique_violation(e: &anyhow::Error) -> bool {
    e.downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .is_some_and(|d| d.code().as_deref() == Some("23505"))
}

/// Whether the caller's files are on the volume this boot uses.
fn resumed(row: &MachineRow, hint: bool) -> bool {
    if row.machine_state() == MachineState::Running {
        row.volume_existed
    } else {
        hint
    }
}

async fn ensure_existing(
    state: &AppState,
    ctx: &AuthCtx,
    existing: MachineRow,
    fields: &MachineFields,
    opts: StartOpts,
) -> LifeResult<Json<serde_json::Value>> {
    if !owns(ctx, existing.org_id, existing.user_id) {
        return Err(AppError(StatusCode::CONFLICT, "that external_id belongs to another user's machine".into()).into());
    }
    // A new image throws a kept root disk away at the next boot (see
    // workerd's `Machines::boot`). Snapshot it first, so what was installed
    // can still be restored. The worker reads the root disk before that boot
    // may start, so the order of the two frames is all it takes.
    if existing.persist_root
        && existing.image != fields.image
        && matches!(existing.machine_state(), MachineState::Stopped | MachineState::Failed)
        && crate::snapshots::enabled(state)
        && crate::snapshots::holder(state, &existing).is_some()
    {
        if let Err(e) = crate::snapshots::take(state, &existing, SnapshotTrigger::Update, None, false).await {
            tracing::warn!(machine = %existing.id, error = format!("{e:#}"), "no snapshot before the image change");
        }
    }
    // Placed as this request asks, not as the machine was stored before it.
    let updated = mdb::update_fields(&state.pool, existing.id, fields).await?;
    ensure_running(state, updated, opts).await
}

/// Start a machine unless it is already live, then optionally wait for it.
async fn ensure_running(state: &AppState, row: MachineRow, opts: StartOpts) -> LifeResult<Json<serde_json::Value>> {
    let mut current = row;
    // A machine still stopping cannot be started until its worker confirms.
    // Wait that out briefly rather than refusing: "start" right after
    // "stop" is exactly what an idle-then-wake cycle does.
    if current.machine_state() == MachineState::Stopping {
        current = wait_until(state, current.id, 30, &[MachineState::Stopped, MachineState::Failed]).await?;
    }
    match current.machine_state() {
        MachineState::Stopped | MachineState::Failed => {
            // Asked before the row is touched: a start nothing can place
            // leaves the machine as it was, now saying why.
            let start = if opts.queue {
                Start::InPlace
            } else {
                match placeable(state, &mut current, opts.relocate).await? {
                    Ok(start) => start,
                    Err(why) => {
                        let mut refused = refusal(state, why.clone(), &current.image).await;
                        if matches!(why, Unplaceable::VolumeHostOffline { .. }) {
                            let snapshot = crate::snapshots::enabled(state)
                                && sdb::latest_ready(&state.pool, current.id).await?.is_some();
                            refused.detail["snapshot_available"] = snapshot.into();
                            if snapshot {
                                refused.message.push_str(
                                    "; start it with \"relocate\": true to restore its latest snapshot on another worker now",
                                );
                            }
                        }
                        mdb::note_refusal(&state.pool, current.id, refused.reason, &refused.message).await?;
                        let now = mdb::get(&state.pool, current.id).await?.unwrap_or(current);
                        return Err(refused.with_machine(&now).into());
                    }
                }
            };
            let scheduled = match start {
                Start::InPlace => mdb::schedule_start(&state.pool, current.id).await?,
                Start::Restore(sid) => {
                    tracing::info!(machine = %current.id, snapshot = %sid, "starting the machine from its latest snapshot on another worker");
                    mdb::schedule_restore(&state.pool, current.id, sid, None, current.cpus, current.memory_mib).await?
                }
            };
            let Some(scheduled) = scheduled else {
                return Err(AppError(StatusCode::CONFLICT, "the machine changed state; retry".into()).into());
            };
            // A restore brings the files back, so they count as resumed.
            let hint = current.volume_worker_id.is_some() || matches!(start, Start::Restore(_));
            place_and_wait(state, scheduled, hint, opts).await
        }
        MachineState::Destroyed => Err(not_found(current.id).into()),
        MachineState::Stopping => Err(Refused::new(StatusCode::CONFLICT, "stopping", "the machine is still stopping; retry")
            .retry_after(Some(5))
            .with_machine(&current)
            .into()),
        // Already live. A boot queued earlier gets another placement try;
        // either way, wait for it.
        _ => {
            let hint = current.volume_worker_id.is_some();
            if current.machine_state() == MachineState::Scheduled && current.worker_id.is_none() {
                let _ = dispatch_one(state, &current).await?;
            }
            let row = wait_boot(state, current.id, current.generation, opts).await?;
            Ok(Json(serde_json::json!({"machine": row.to_api(), "resumed": resumed(&row, hint)})))
        }
    }
}

/// Poll until the machine reaches one of `states`, or `wait` seconds pass.
/// Returns the row either way.
async fn wait_until(state: &AppState, id: Uuid, wait: u64, states: &[MachineState]) -> ApiResult<MachineRow> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let row = mdb::get(&state.pool, id).await?.ok_or_else(|| not_found(id))?;
        if wait == 0 || states.contains(&row.machine_state()) || tokio::time::Instant::now() >= deadline {
            return Ok(row);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// How a start behaves when it cannot happen at once.
#[derive(Clone, Copy)]
struct StartOpts {
    /// Seconds to wait for the boot to settle; 0 returns once it is placed.
    wait: u64,
    /// Keep a boot nothing can place scheduled, for the dispatcher to retry,
    /// instead of refusing it: the behaviour before fail-fast, now opt-in.
    queue: bool,
    /// When the worker holding the volume is offline, restore the latest
    /// snapshot on another worker now, rather than refusing until the grace
    /// period is up.
    relocate: bool,
}

/// How a stopped machine is going to start.
#[derive(Clone, Copy)]
enum Start {
    /// On the worker holding its volume (or anywhere, if it has none).
    InPlace,
    /// From this snapshot, on whichever worker can take it.
    Restore(Uuid),
}

/// Put a just-scheduled boot on a worker and wait for it to settle.
///
/// Placement happens here, in the request, rather than being left to the
/// dispatcher: that is what lets a boot nothing can take come straight back
/// with a reason.
async fn place_and_wait(
    state: &AppState,
    row: MachineRow,
    hint: bool,
    opts: StartOpts,
) -> LifeResult<Json<serde_json::Value>> {
    if let Err(why) = dispatch_one(state, &row).await? {
        if let Some(refused) = give_up(state, &row, why, opts).await? {
            return Err(refused.into());
        }
    }
    let row = wait_boot(state, row.id, row.generation, opts).await?;
    Ok(Json(serde_json::json!({"machine": row.to_api(), "resumed": resumed(&row, hint)})))
}

/// A scheduled boot nobody can take. Queued, it waits with the reason on
/// the row. Otherwise it goes back to stopped and the refusal is returned --
/// unless a dispatcher placed it meanwhile, when there is nothing to refuse.
async fn give_up(
    state: &AppState,
    row: &MachineRow,
    why: Unplaceable,
    opts: StartOpts,
) -> anyhow::Result<Option<Refused>> {
    if opts.queue {
        mdb::note_waiting(&state.pool, row.id, why.reason()).await?;
        return Ok(None);
    }
    let refused = refusal(state, why, &row.image).await;
    if !mdb::unschedule(&state.pool, row.id, row.generation, refused.reason, &refused.message).await? {
        return Ok(None);
    }
    Ok(Some(match mdb::get(&state.pool, row.id).await? {
        Some(m) => refused.with_machine(&m),
        None => refused,
    }))
}

/// Wait up to `opts.wait` seconds for a boot to settle, returning as soon as
/// it has an answer: running, or failed -- which comes back at once as
/// `boot_failed` with the worker's reason, not as a row that says failed
/// after the whole wait.
async fn wait_boot(state: &AppState, id: Uuid, generation: i64, opts: StartOpts) -> LifeResult<MachineRow> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(opts.wait);
    let mut placed = false;
    let mut replaced = false;
    loop {
        let row = mdb::get(&state.pool, id).await?.ok_or_else(|| not_found(id))?;
        if row.generation != generation {
            return Ok(row); // a later start, or a stop, took over
        }
        match row.machine_state() {
            MachineState::Failed => return Err(boot_failed(&row).into()),
            MachineState::Scheduled | MachineState::Restoring | MachineState::Booting => {}
            _ => return Ok(row),
        }
        if row.worker_id.is_some() {
            placed = true;
        } else if placed && !replaced {
            // Its worker disconnected before booting it, and the assignment
            // died with the link. Place it once more, then stop guessing.
            replaced = true;
            if let Err(why) = dispatch_one(state, &row).await? {
                if let Some(r) = give_up(state, &row, why, opts).await? {
                    let message =
                        format!("the worker this machine was placed on went away before booting it, and {}", r.message);
                    return Err(Refused { reason: "worker_lost", message, ..r }.into());
                }
            }
        }
        if opts.wait == 0 || tokio::time::Instant::now() >= deadline {
            return Ok(row);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The worker tried and the boot failed: say so, with its reason.
fn boot_failed(row: &MachineRow) -> Refused {
    let message = row.error.clone().unwrap_or_else(|| "the machine failed to boot".into());
    Refused::new(StatusCode::BAD_GATEWAY, "boot_failed", message)
        .detail(serde_json::json!({"worker_reason": row.last_reason, "worker_error": row.error}))
        .with_machine(row)
}

#[derive(Deserialize, Default)]
struct WaitReq {
    wait_s: Option<u64>,
    #[serde(default)]
    queue: bool,
    #[serde(default)]
    relocate: bool,
}

async fn start_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    body: Option<Json<WaitReq>>,
) -> LifeResult<Json<serde_json::Value>> {
    let row = load_owned(&state, &ctx, id).await?;
    let req = body.map(|b| b.0).unwrap_or_default();
    let opts = StartOpts { wait: req.wait_s.unwrap_or(0).min(MAX_WAIT_S), queue: req.queue, relocate: req.relocate };
    ensure_running(&state, row, opts).await
}

#[derive(Deserialize, Default)]
struct RestoreReq {
    /// A snapshot id, or `"latest"` (the default).
    snapshot_id: Option<String>,
    /// Land on this worker; on any that can take it when absent.
    worker_id: Option<Uuid>,
    /// Size to boot at; the machine's own when absent.
    cpus: Option<i32>,
    memory_mib: Option<i32>,
    /// Stop a live machine first rather than refusing.
    #[serde(default)]
    stop: bool,
    wait_s: Option<u64>,
    #[serde(default)]
    queue: bool,
}

/// `POST /v1/machines/{id}/restore`: boot a machine from one of its
/// snapshots, on whichever worker can take it.
async fn restore_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    body: Option<Json<RestoreReq>>,
) -> LifeResult<Json<serde_json::Value>> {
    let mut row = load_owned(&state, &ctx, id).await?;
    let req = body.map(|b| b.0).unwrap_or_default();
    if !crate::snapshots::enabled(&state) {
        return Err(snapshots_unavailable().into());
    }
    let snapshot = match req.snapshot_id.as_deref() {
        None | Some("latest") => sdb::latest_ready(&state.pool, id).await?,
        Some(s) => {
            let sid = Uuid::parse_str(s).map_err(|_| bad_request("snapshot_id must be a uuid or \"latest\""))?;
            sdb::get(&state.pool, sid).await?.filter(|s| s.machine_id == id)
        }
    }
    .ok_or_else(|| Refused::new(StatusCode::NOT_FOUND, "no_snapshot", "the machine has no such ready snapshot"))?;
    if snapshot.state != "ready" {
        return Err(Refused::new(
            StatusCode::CONFLICT,
            "snapshot_not_ready",
            format!("snapshot {} is {}, not ready", snapshot.id, snapshot.state),
        )
        .into());
    }
    if req.stop && matches!(row.machine_state(), MachineState::Running | MachineState::Booting | MachineState::Scheduled) {
        stop(&state, &row).await?;
    }
    if matches!(row.machine_state(), MachineState::Stopping) || req.stop {
        row = wait_until(&state, id, 60, &[MachineState::Stopped, MachineState::Failed]).await?;
    }
    match row.machine_state() {
        MachineState::Stopped | MachineState::Failed => {}
        MachineState::Destroyed => return Err(not_found(id).into()),
        s => {
            return Err(Refused::new(
                StatusCode::CONFLICT,
                "running",
                format!("the machine is {}; stop it first, or pass \"stop\": true", s.as_str()),
            )
            .with_machine(&row)
            .into())
        }
    }
    let cfg = &state.cfg;
    let cpus = req.cpus.map_or(row.cpus, |c| c.clamp(1, cfg.machine_max_cpus as i32));
    let memory_mib = req.memory_mib.map_or(row.memory_mib, |m| m.clamp(512, cfg.machine_max_memory_mib as i32));
    let opts = StartOpts { wait: req.wait_s.unwrap_or(0).min(MAX_WAIT_S), queue: req.queue, relocate: false };
    // As for a start: refused before anything changes.
    if !opts.queue {
        let p = placement_for(row.engine(), cpus, memory_mib, &row.image, req.worker_id, true);
        if let Err(why) = state.workers.diagnose(&p) {
            let refused = refusal(&state, why, &row.image).await;
            mdb::note_refusal(&state.pool, id, refused.reason, &refused.message).await?;
            return Err(refused.with_machine(&row).into());
        }
    }
    let Some(scheduled) = mdb::schedule_restore(&state.pool, id, snapshot.id, req.worker_id, cpus, memory_mib).await?
    else {
        return Err(AppError(StatusCode::CONFLICT, "the machine changed state; retry".into()).into());
    };
    crate::db::audit(
        &state.pool,
        Some(ctx.org_id),
        ctx.user_id,
        "machine.restore",
        &id.to_string(),
        serde_json::json!({"snapshot_id": snapshot.id, "worker_id": req.worker_id}),
    )
    .await
    .ok();
    place_and_wait(&state, scheduled, true, opts).await
}

// -------------------------------------------------------------- snapshots

fn snapshots_unavailable() -> Refused {
    Refused::new(
        StatusCode::CONFLICT,
        "snapshots_unavailable",
        "this deployment does not take snapshots: it needs object storage (PUKU_R2_*) and PUKU_SECRET_KEY",
    )
}

#[derive(Deserialize, Default)]
struct SnapshotReq {
    label: Option<String>,
    /// Kept whatever the machine's `keep` says.
    #[serde(default)]
    pinned: bool,
    wait_s: Option<u64>,
}

/// `POST /v1/machines/{id}/snapshots`: capture the machine's disks now --
/// live while it runs, clean while it is stopped.
async fn create_snapshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    body: Option<Json<SnapshotReq>>,
) -> LifeResult<(StatusCode, Json<serde_json::Value>)> {
    let row = load_owned(&state, &ctx, id).await?;
    let req = body.map(|b| b.0).unwrap_or_default();
    if !crate::snapshots::enabled(&state) {
        return Err(snapshots_unavailable().into());
    }
    if !crate::snapshots::has_disks(&row) {
        return Err(Refused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "nothing_to_snapshot",
            "the machine has no volume and no persistent root disk",
        )
        .into());
    }
    match row.machine_state() {
        MachineState::Running | MachineState::Stopped | MachineState::Failed => {}
        MachineState::Destroyed => return Err(not_found(id).into()),
        s => {
            return Err(Refused::new(
                StatusCode::CONFLICT,
                "busy",
                format!("the machine is {}; snapshot it once it is running or stopped", s.as_str()),
            )
            .retry_after(Some(5))
            .with_machine(&row)
            .into())
        }
    }
    let Some(home) = row.worker_id.or(row.volume_worker_id) else {
        return Err(Refused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "nothing_to_snapshot",
            "the machine has never booted, so it has no disks yet",
        )
        .into());
    };
    let Some(worker) = state.workers.get(home) else {
        return Err(refusal(&state, Unplaceable::VolumeHostOffline { worker_id: home }, &row.image)
            .await
            .with_machine(&row)
            .into());
    };
    if !worker.features.iter().any(|f| f == FEATURE_SNAPSHOTS) {
        return Err(Refused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "snapshots_unsupported",
            format!("worker {} runs a workerd without snapshots; upgrade it", worker.name),
        )
        .into());
    }
    let taken =
        crate::snapshots::take(&state, &row, SnapshotTrigger::Manual, req.label.as_deref(), req.pinned).await?;
    let taken = wait_snapshot(&state, taken, req.wait_s.unwrap_or(0).min(MAX_WAIT_S)).await?;
    let status = if taken.is_open() { StatusCode::ACCEPTED } else { StatusCode::OK };
    Ok((status, Json(taken.to_api())))
}

/// Poll a snapshot until it is no longer being taken, or `wait` seconds pass.
async fn wait_snapshot(state: &AppState, mut s: SnapshotRow, wait: u64) -> anyhow::Result<SnapshotRow> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    while s.is_open() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        s = sdb::get(&state.pool, s.id).await?.ok_or_else(|| anyhow::anyhow!("the snapshot vanished"))?;
    }
    Ok(s)
}

async fn list_snapshots(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    load_owned(&state, &ctx, id).await?;
    let rows = sdb::list(&state.pool, id, 100).await?;
    Ok(Json(rows.iter().map(SnapshotRow::to_api).collect()))
}

async fn load_snapshot(state: &AppState, machine_id: Uuid, sid: Uuid) -> ApiResult<SnapshotRow> {
    match sdb::get(&state.pool, sid).await? {
        Some(s) if s.machine_id == machine_id && s.state != "deleted" => Ok(s),
        _ => Err(AppError(StatusCode::NOT_FOUND, format!("snapshot {sid} not found"))),
    }
}

async fn get_snapshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path((id, sid)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    load_owned(&state, &ctx, id).await?;
    Ok(Json(load_snapshot(&state, id, sid).await?.to_api()))
}

/// Queue a snapshot's objects for deletion. Idempotent.
async fn delete_snapshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path((id, sid)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    load_owned(&state, &ctx, id).await?;
    let s = load_snapshot(&state, id, sid).await?;
    if s.is_open() {
        return Err(AppError(
            StatusCode::CONFLICT,
            "the snapshot is still being taken; delete it once it is ready or failed".into(),
        ));
    }
    sdb::mark_deleting(&state.pool, sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    body: Option<Json<WaitReq>>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let row = load_owned(&state, &ctx, id).await?;
    if row.machine_state() == MachineState::Destroyed {
        return Err(not_found(id));
    }
    let snapshot = stop(&state, &row).await?;
    let wait = body.map(|b| b.0.wait_s.unwrap_or(0)).unwrap_or(0).min(MAX_WAIT_S);
    let row = wait_until(&state, id, wait, &[MachineState::Stopped, MachineState::Failed]).await?;
    let mut body = serde_json::json!({"machine": row.to_api()});
    if let Some(s) = match snapshot {
        Some(sid) => sdb::get(&state.pool, sid).await?,
        None => None,
    } {
        body["snapshot"] = s.to_api();
    }
    Ok((StatusCode::ACCEPTED, Json(body)))
}

/// Ask a machine's worker to stop it, with the snapshot its policy asks for
/// on stop. Used by the API and the idle sweep. Returns that snapshot's id.
async fn stop(state: &AppState, row: &MachineRow) -> anyhow::Result<Option<Uuid>> {
    let Some(stopping) = mdb::request_stop(&state.pool, row.id).await? else {
        return Ok(None); // already stopped, failed or stopping
    };
    let mut ordered = None;
    if let Some(worker_id) = stopping.worker_id {
        let delivered = match state.workers.get(worker_id) {
            Some(w) => {
                // The disks are about to go quiet: the moment for a clean one.
                let snapshot = if SnapshotPolicy::of(&stopping).on_stop {
                    crate::snapshots::order_for(state, &stopping, SnapshotTrigger::Stop, &w).await
                } else {
                    None
                };
                ordered = snapshot.as_ref().map(|o| o.snapshot_id);
                let frame =
                    Down::StopMachine { machine_id: row.id, generation: stopping.generation as u64, snapshot };
                w.send(frame)
            }
            None => false,
        };
        if !delivered {
            if let Some(sid) = ordered.take() {
                sdb::set_state(&state.pool, sid, "failed", Some("the worker went away before it got the order"))
                    .await?;
            }
            // The worker is away. Its VM, if any, is stopped on reconnect
            // (see workerlink), so the row can say stopped now.
            mdb::apply_report(&state.pool, row.id, worker_id, stopping.generation, MachineState::Stopped, None, None, false)
                .await?;
            mdb::close_runs(&state.pool, row.id).await?;
        }
    }
    Ok(ordered)
}

#[derive(Deserialize, Default)]
struct DeleteQuery {
    /// Take a last snapshot first. Defaults to the machine's
    /// `snapshots.before_destroy`.
    snapshot: Option<String>,
    /// Delete every snapshot of the machine as well, now.
    purge: Option<String>,
}

async fn delete_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let row = load_owned(&state, &ctx, id).await?;
    let purge = truthy(&q.purge);
    let final_wanted = !purge
        && match &q.snapshot {
            Some(_) => truthy(&q.snapshot),
            None => SnapshotPolicy::of(&row).before_destroy,
        };
    if let Some(before) = mdb::mark_destroyed(&state.pool, id).await? {
        // Whoever has the VM or the volume. A worker that is away reaps it
        // on reconnect: destroyed machines come back as `reapable_machines`.
        // The one holding the disks takes the last snapshot before deleting.
        let holder = before.worker_id.or(before.volume_worker_id);
        let mut told = Vec::new();
        for worker_id in [before.worker_id, before.volume_worker_id].into_iter().flatten() {
            if told.contains(&worker_id) {
                continue;
            }
            told.push(worker_id);
            let Some(w) = state.workers.get(worker_id) else { continue };
            let final_snapshot = if final_wanted && holder == Some(worker_id) {
                crate::snapshots::order_for(&state, &before, SnapshotTrigger::Destroy, &w).await
            } else {
                None
            };
            w.send(Down::DestroyMachine { machine_id: id, final_snapshot });
        }
        if purge {
            sdb::mark_all_deleting(&state.pool, id).await?;
        }
        mdb::close_runs(&state.pool, id).await?;
        crate::db::audit(
            &state.pool,
            Some(ctx.org_id),
            ctx.user_id,
            "machine.destroy",
            &id.to_string(),
            serde_json::json!({"purge": purge, "final_snapshot": final_wanted}),
        )
        .await
        .ok();
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn touch_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    load_owned(&state, &ctx, id).await?;
    mdb::touch(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(load_owned(&state, &ctx, id).await?.to_api()))
}

#[derive(Deserialize)]
struct ListQuery {
    external_id: Option<String>,
    state: Option<String>,
    limit: Option<i64>,
}

async fn list_machines(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let user = if ctx.admin { None } else { ctx.user_id };
    let rows = mdb::list(
        &state.pool,
        ctx.org_id,
        user,
        q.external_id.as_deref(),
        q.state.as_deref(),
        q.limit.unwrap_or(50).clamp(1, 500),
    )
    .await?;
    Ok(Json(rows.iter().map(MachineRow::to_api).collect()))
}

// ------------------------------------------------------------ dispatch

/// Place scheduled machines on workers. Called from `api::dispatch_pending`,
/// so every place that dispatches sessions dispatches machines too.
///
/// What waits here is only what was queued on purpose (`queue: true`, or a
/// boot whose worker went away): a start the fleet cannot serve is refused
/// in the request instead. A queued machine carries the reason it waits.
pub async fn dispatch_machines(state: &AppState) -> anyhow::Result<()> {
    for m in mdb::dispatchable(&state.pool).await? {
        if let Err(why) = dispatch_one(state, &m).await? {
            mdb::note_waiting(&state.pool, m.id, why.reason()).await?;
        }
    }
    Ok(())
}

/// What a machine of these dimensions needs from a worker.
fn placement_for(
    engine: Engine,
    cpus: i32,
    memory_mib: i32,
    image: &str,
    pinned: Option<Uuid>,
    restoring: bool,
) -> Placement {
    let cpus = cpus.max(1) as u32;
    Placement {
        engine,
        pinned,
        features: if restoring { RESTORE_FEATURES } else { MACHINE_FEATURES },
        slots: slots_for(cpus, memory_mib.max(0) as u32),
        cpus,
        // Only an engine that boots a prepared disk needs it staged first;
        // msb pulls on demand.
        image_key: (engine == Engine::CloudHypervisor).then(|| image_key(image)),
        disk_mib: MIN_FREE_DISK_MIB,
    }
}

fn row_placement(m: &MachineRow) -> Placement {
    // `restore_snapshot_id` outlives the restored boot (it is what the API
    // reports as `restored_from`), so only a boot still waiting to be placed
    // is actually restoring.
    let restoring = m.restore_snapshot_id.is_some() && m.machine_state() == MachineState::Scheduled;
    placement_for(m.engine(), m.cpus, m.memory_mib, &m.image, m.volume_worker_id, restoring)
}

/// Why nobody took a placement `pick` just failed on.
fn nobody(state: &AppState, p: &Placement) -> Unplaceable {
    // `Ok` here means another request took the last room in between.
    state.workers.diagnose(p).err().unwrap_or(Unplaceable::CapacityFull {
        requested_slots: p.slots(),
        best_free_slots: 0,
        workers: state.workers.len(),
    })
}

/// How a stopped machine could start now, if at all.
///
/// When the worker holding its volume is away, the machine's latest
/// snapshot can bring it back on another worker: at once when the caller
/// asks to relocate, and on its own once that worker has been gone past the
/// grace period. With no snapshot, past the grace period it starts with an
/// empty volume, as it always has.
async fn placeable(state: &AppState, m: &mut MachineRow, relocate: bool) -> anyhow::Result<Result<Start, Unplaceable>> {
    let first = state.workers.diagnose(&row_placement(m));
    let Err(Unplaceable::VolumeHostOffline { worker_id }) = first else {
        return Ok(first.map(|()| Start::InPlace));
    };
    let gone = host_gone_past_grace(state, worker_id).await?;
    let latest = if crate::snapshots::enabled(state) { sdb::latest_ready(&state.pool, m.id).await? } else { None };
    match latest {
        Some(s) if relocate || gone => {
            let p = placement_for(m.engine(), m.cpus, m.memory_mib, &m.image, None, true);
            Ok(state.workers.diagnose(&p).map(|()| Start::Restore(s.id)))
        }
        _ if gone => {
            tracing::warn!(machine = %m.id, worker = %worker_id, "volume host gone and no snapshot; starting with a fresh volume");
            mdb::forget_volume(&state.pool, m.id).await?;
            m.volume_worker_id = None;
            Ok(state.workers.diagnose(&row_placement(m)).map(|()| Start::InPlace))
        }
        _ => Ok(Err(Unplaceable::VolumeHostOffline { worker_id })),
    }
}

/// Put one scheduled machine on a worker now.
///
/// `Ok(Ok(()))` also when another dispatcher placed it first, or its spec
/// could not be built (the row then says failed): either way there is
/// nothing left to place.
async fn dispatch_one(state: &AppState, m: &MachineRow) -> anyhow::Result<Result<(), Unplaceable>> {
    let mut m = m.clone();
    let worker = loop {
        let placement = row_placement(&m);
        if let Some(w) = state.workers.pick(&placement) {
            break w;
        }
        // A restore pinned to a worker waits for that worker; a boot pinned
        // to its volume waits for its volume host, until the grace is up.
        let Some(pinned) = m.volume_worker_id else { return Ok(Err(nobody(state, &placement))) };
        if m.restore_snapshot_id.is_some() || !host_gone_past_grace(state, pinned).await? {
            return Ok(Err(nobody(state, &placement)));
        }
        let latest = if crate::snapshots::enabled(state) { sdb::latest_ready(&state.pool, m.id).await? } else { None };
        match latest {
            Some(s) => {
                tracing::warn!(machine = %m.id, worker = %pinned, snapshot = %s.id, "volume host gone; restoring the latest snapshot elsewhere");
                if !mdb::relocate_scheduled(&state.pool, m.id, m.generation, s.id).await? {
                    return Ok(Ok(())); // placed or moved on meanwhile
                }
                m.restore_snapshot_id = Some(s.id);
                m.stale_worker_id = Some(pinned);
            }
            None => {
                tracing::warn!(machine = %m.id, worker = %pinned, "volume host gone and no snapshot; re-placing with a fresh volume");
                mdb::forget_volume(&state.pool, m.id).await?;
            }
        }
        m.volume_worker_id = None;
    };
    if !mdb::assign_worker(&state.pool, m.id, worker.worker_id, m.generation).await? {
        return Ok(Ok(()));
    }
    let spec = match machine_spec(state, &m).await {
        Ok(spec) => spec,
        Err(e) => {
            let error = format!("{e:#}");
            mdb::apply_report(
                &state.pool,
                m.id,
                worker.worker_id,
                m.generation,
                MachineState::Failed,
                Some(&error),
                Some("invalid_spec"),
                false,
            )
            .await?;
            return Ok(Ok(()));
        }
    };
    if !worker.send(Down::AssignMachine { spec }) {
        mdb::unassign(&state.pool, m.id, m.generation).await?;
        return Ok(Err(nobody(state, &row_placement(&m))));
    }
    tracing::info!(machine = %m.id, worker = %worker.name, engine = %m.engine, "machine dispatched");
    Ok(Ok(()))
}

/// Whether a machine's volume host has been gone past the grace period:
/// long enough that starting elsewhere -- from the latest snapshot, or with
/// an empty volume and `resumed: false` -- beats waiting. Unlike a session
/// there is nothing to fail: the caller holds the authoritative copy of what
/// was on it.
async fn host_gone_past_grace(state: &AppState, worker_id: Uuid) -> anyhow::Result<bool> {
    if state.workers.get(worker_id).is_some() {
        return Ok(false);
    }
    let gone_for = match crate::db::worker_presence(&state.pool, worker_id).await? {
        Some((_, _, Some(hb))) => chrono::Utc::now() - hb,
        _ => VOLUME_HOST_GRACE + chrono::Duration::seconds(1),
    };
    Ok(gone_for > VOLUME_HOST_GRACE)
}

async fn machine_spec(state: &AppState, m: &MachineRow) -> anyhow::Result<MachineSpec> {
    let mut env: BTreeMap<String, String> = serde_json::from_value(m.env.clone()).unwrap_or_default();
    if let Some(enc) = mdb::secret_env(&state.pool, m.id).await? {
        let secrets = state
            .secrets
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("secret_env is stored but PUKU_SECRET_KEY is not configured"))?;
        let plain: BTreeMap<String, String> = serde_json::from_str(&secrets.decrypt(&enc)?)?;
        env.extend(plain);
    }
    Ok(MachineSpec {
        machine_id: m.id,
        name: m.name.clone(),
        engine: m.engine(),
        image: m.image.clone(),
        cpus: m.cpus.clamp(1, u8::MAX as i32) as u8,
        memory_mib: m.memory_mib as u32,
        expose: m.expose.iter().map(|p| *p as u16).collect(),
        env,
        entrypoint: m.entrypoint.clone().and_then(|v| serde_json::from_value(v).ok()),
        volume: m.volume.clone().and_then(|v| serde_json::from_value(v).ok()),
        max_duration_s: m.max_duration_s as u32,
        generation: m.generation as u64,
        persist_root: m.persist_root,
        restore: match m.restore_snapshot_id {
            Some(sid) if m.machine_state() == MachineState::Scheduled => {
                Some(crate::snapshots::restore_order(state, sid).await?)
            }
            _ => None,
        },
    })
}

/// Stop machines idle past their own timeout. Runs forever.
pub fn spawn_idle_sweep(state: AppState) {
    let every = Duration::from_secs(state.cfg.machine_idle_sweep_s.max(5));
    puku_observability::supervise("machine_idle_sweep", move || {
        let state = state.clone();
        async move {
            loop {
                tokio::time::sleep(every).await;
                match mdb::idle(&state.pool).await {
                    Ok(rows) => {
                        for m in rows {
                            tracing::info!(machine = %m.id, "idle timeout; stopping");
                            if let Err(e) = stop(&state, &m).await {
                                tracing::warn!(machine = %m.id, error = format!("{e:#}"), "idle stop failed");
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "idle sweep failed"),
                }
            }
        }
    });
}

// -------------------------------------------------------------- streams

/// A running machine's worker, or why there is none.
fn running_on(row: &MachineRow) -> ApiResult<Uuid> {
    match (row.machine_state(), row.worker_id) {
        (MachineState::Running, Some(w)) => Ok(w),
        (s, _) => Err(AppError(
            StatusCode::CONFLICT,
            format!("machine {} is {}, not running", row.id, s.as_str()),
        )),
    }
}

#[derive(Deserialize)]
struct ExecReq {
    argv: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    user: Option<String>,
    timeout_ms: Option<u64>,
    stdin: Option<String>,
    stdin_b64: Option<String>,
}

async fn exec(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Json(req): Json<ExecReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = load_owned(&state, &ctx, id).await?;
    let worker = running_on(&row)?;
    if req.argv.is_empty() {
        return Err(bad_request("argv must name a command"));
    }
    let stdin: Option<Vec<u8>> = match (req.stdin, req.stdin_b64) {
        (Some(s), _) => Some(s.into_bytes()),
        (None, Some(b)) => Some(
            base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|e| bad_request(format!("stdin_b64: {e}")))?,
        ),
        _ => None,
    };
    let target = StreamTarget::Exec {
        argv: req.argv,
        cwd: req.cwd,
        env: req.env,
        user: req.user,
        timeout_ms: req.timeout_ms.unwrap_or(DEFAULT_EXEC_TIMEOUT_MS).clamp(1, MAX_EXEC_TIMEOUT_MS),
        stdin: stdin.is_some(),
    };
    let mut ws = datalink::open(&state, worker, id, target).await?;
    if let Some(bytes) = stdin {
        datalink::send_body(&mut ws, axum::body::Body::from(bytes)).await?;
    }
    let _ = mdb::touch(&state.pool, id).await;
    match datalink::next_msg(&mut ws).await {
        Some(DataMsg::ExecResult { code, stdout, stderr, timed_out, truncated }) => Ok(Json(serde_json::json!({
            "code": code, "stdout": stdout, "stderr": stderr,
            "timed_out": timed_out, "truncated": truncated,
        }))),
        Some(DataMsg::Error { status, message }) => Err(StreamError { status, message }.into()),
        _ => Err(AppError(StatusCode::BAD_GATEWAY, "the worker closed the stream mid-exec".into())),
    }
}

#[derive(Deserialize)]
struct FilesQuery {
    path: String,
    max_bytes: Option<u64>,
    list: Option<String>,
    recursive: Option<String>,
    mode: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes" | ""))
}

async fn read_files(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Query(q): Query<FilesQuery>,
) -> ApiResult<Response> {
    guest_path(&q.path)?;
    let row = load_owned(&state, &ctx, id).await?;
    let worker = running_on(&row)?;
    let _ = mdb::touch(&state.pool, id).await;
    if truthy(&q.list) {
        let target = StreamTarget::List { path: q.path, recursive: truthy(&q.recursive) };
        let mut ws = datalink::open(&state, worker, id, target).await?;
        return match datalink::next_msg(&mut ws).await {
            Some(DataMsg::Listing { entries }) => Ok(Json(entries).into_response()),
            Some(DataMsg::Error { status, message }) => Err(StreamError { status, message }.into()),
            _ => Err(AppError(StatusCode::BAD_GATEWAY, "the worker closed the stream".into())),
        };
    }
    let target = StreamTarget::FileRead { path: q.path, max_bytes: q.max_bytes };
    let ws = datalink::open(&state, worker, id, target).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        datalink::body_from(ws),
    )
        .into_response())
}

async fn write_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Query(q): Query<FilesQuery>,
    body: axum::body::Body,
) -> ApiResult<StatusCode> {
    guest_path(&q.path)?;
    let mode = match q.mode.as_deref() {
        None => 0o644,
        Some(m) => u32::from_str_radix(m.trim_start_matches("0o"), 8)
            .ok()
            .filter(|m| *m <= 0o7777)
            .ok_or_else(|| bad_request(format!("mode {m:?} is not octal permissions")))?,
    };
    let row = load_owned(&state, &ctx, id).await?;
    let worker = running_on(&row)?;
    let mut ws = datalink::open(&state, worker, id, StreamTarget::FileWrite { path: q.path, mode }).await?;
    datalink::send_body(&mut ws, body).await?;
    let _ = mdb::touch(&state.pool, id).await;
    finish_write(&mut ws).await
}

async fn finish_write(ws: &mut axum::extract::ws::WebSocket) -> ApiResult<StatusCode> {
    match datalink::next_msg(ws).await {
        Some(DataMsg::Done) => Ok(StatusCode::NO_CONTENT),
        Some(DataMsg::Error { status, message }) => Err(StreamError { status, message }.into()),
        _ => Err(AppError(StatusCode::BAD_GATEWAY, "the worker closed the stream mid-write".into())),
    }
}

/// `?path=/x&exclude=a&exclude=b` -- repeated keys, which serde's Query does
/// not collect, so the raw query is walked by hand.
fn archive_query(query: Option<&str>) -> (Option<String>, Vec<String>, bool) {
    let mut path = None;
    let mut excludes = Vec::new();
    let mut replace = false;
    for (k, v) in url_pairs(query.unwrap_or("")) {
        match k.as_str() {
            "path" => path = Some(v),
            "exclude" => excludes.push(v),
            "replace" => replace = matches!(v.as_str(), "1" | "true" | "yes" | ""),
            _ => {}
        }
    }
    (path, excludes, replace)
}

fn url_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// `application/x-www-form-urlencoded` decoding. A `%` not followed by two
/// hex digits is kept literally rather than rejected.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = b
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn get_archive(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    req: Request,
) -> ApiResult<Response> {
    let (path, excludes, _) = archive_query(req.uri().query());
    let path = path.ok_or_else(|| bad_request("path is required"))?;
    guest_path(&path)?;
    let row = load_owned(&state, &ctx, id).await?;
    let worker = running_on(&row)?;
    let _ = mdb::touch(&state.pool, id).await;
    let ws = datalink::open(&state, worker, id, StreamTarget::ArchiveGet { path, excludes }).await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/gzip")], datalink::body_from(ws)).into_response())
}

async fn put_archive(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    req: Request,
) -> ApiResult<StatusCode> {
    let (path, _, replace) = archive_query(req.uri().query());
    let path = path.ok_or_else(|| bad_request("path is required"))?;
    guest_path(&path)?;
    let row = load_owned(&state, &ctx, id).await?;
    let worker = running_on(&row)?;
    let mut ws = datalink::open(&state, worker, id, StreamTarget::ArchivePut { path, replace }).await?;
    datalink::send_body(&mut ws, req.into_body()).await?;
    let _ = mdb::touch(&state.pool, id).await;
    finish_write(&mut ws).await
}

// ---------------------------------------------------------------- links

#[derive(Deserialize)]
struct LinkReq {
    port: u16,
    path: Option<String>,
    query: Option<String>,
    ttl_s: Option<i64>,
}

async fn create_link(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Json(req): Json<LinkReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = load_owned(&state, &ctx, id).await?;
    if !row.exposes(req.port) {
        return Err(AppError(StatusCode::FORBIDDEN, format!("port {} is not exposed", req.port)));
    }
    let path = req.path.unwrap_or_else(|| "/".into());
    if !path.starts_with('/') || path.contains("..") || path.contains('?') || path.contains('#') {
        return Err(bad_request("path must be absolute, without `..`, `?` or `#`"));
    }
    let ttl = req.ttl_s.unwrap_or(3600).clamp(1, 86_400);
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(ttl);
    let cap = state.links.mint(id, req.port, expires_at.timestamp());
    let mut url = format!("{}/v1/links/{cap}{path}", state.cfg.links_base.trim_end_matches('/'));
    if let Some(q) = req.query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q.trim_start_matches('?'));
    }
    Ok(Json(serde_json::json!({"url": url, "expires_at": expires_at})))
}

// ---------------------------------------------------------------- proxy

async fn port_proxy_root(
    state: State<AppState>,
    ctx: Extension<AuthCtx>,
    Path((id, port)): Path<(Uuid, u16)>,
    req: Request,
) -> ApiResult<Response> {
    port_proxy(state, ctx, Path((id, port, String::new())), req).await
}

async fn port_proxy(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path((id, port, rest)): Path<(Uuid, u16, String)>,
    req: Request,
) -> ApiResult<Response> {
    let row = load_owned(&state, &ctx, id).await?;
    if !row.exposes(port) {
        return Err(AppError(StatusCode::FORBIDDEN, format!("port {port} is not exposed")));
    }
    proxy(&state, &row, port, &rest, req).await
}

async fn link_proxy_root(state: State<AppState>, Path(cap): Path<String>, req: Request) -> ApiResult<Response> {
    link_proxy(state, Path((cap, String::new())), req).await
}

async fn link_proxy(
    State(state): State<AppState>,
    Path((cap, rest)): Path<(String, String)>,
    req: Request,
) -> ApiResult<Response> {
    // Every failure is the same 404: a probe learns nothing about which
    // machines exist or which links have expired.
    let gone = || AppError(StatusCode::NOT_FOUND, "link not found or expired".into());
    let (id, port) = state.links.verify(&cap, chrono::Utc::now().timestamp()).ok_or_else(gone)?;
    let row = mdb::get(&state.pool, id).await?.ok_or_else(gone)?;
    if !row.exposes(port) {
        return Err(gone());
    }
    let mut resp = proxy(&state, &row, port, &rest, req).await?;
    resp.headers_mut().insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    Ok(resp)
}

/// Headers that describe one hop and must not be forwarded, plus the
/// caller's credentials, which are for the control plane and never for
/// whatever runs inside the guest.
const STRIP_REQUEST: [&str; 9] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "proxy-authenticate",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "host",
];

/// Forwarded into the guest as its `Authorization` header.
const GUEST_AUTHORIZATION: &str = "x-guest-authorization";

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
}

/// Forward one HTTP request (or WebSocket upgrade) to a guest port over the
/// data plane.
async fn proxy(state: &AppState, row: &MachineRow, port: u16, rest: &str, mut req: Request) -> ApiResult<Response> {
    let worker = running_on(row)?;
    let upgrade = is_upgrade(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));

    let path = format!("/{}", rest.trim_start_matches('/'));
    let path_and_query = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let (mut parts, body) = req.into_parts();
    parts.uri = path_and_query
        .parse()
        .map_err(|_| bad_request("that path cannot be forwarded"))?;
    parts.version = axum::http::Version::HTTP_11;
    // `Authorization` on this request is the caller's credential for *us*.
    // A service in the guest that wants its own bearer (control.py) gets it
    // from `x-guest-authorization`, which becomes the guest's
    // `Authorization` only after the caller's has been dropped.
    let guest_auth = parts.headers.remove(GUEST_AUTHORIZATION);
    for h in STRIP_REQUEST {
        parts.headers.remove(h);
    }
    if let Some(v) = guest_auth {
        parts.headers.insert(axum::http::header::AUTHORIZATION, v);
    }
    if !upgrade {
        parts.headers.remove(axum::http::header::CONNECTION);
        parts.headers.remove(axum::http::header::UPGRADE);
    }
    parts.headers.insert(
        axum::http::header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{port}")).expect("valid host"),
    );
    // Strip query-string credentials the same way: `api_key=` is how a
    // WebSocket client authenticates to us.
    if let Some(q) = parts.uri.query() {
        if q.split('&').any(|p| p.starts_with("api_key=")) {
            let kept: Vec<&str> = q.split('&').filter(|p| !p.starts_with("api_key=")).collect();
            let pq = if kept.is_empty() {
                parts.uri.path().to_string()
            } else {
                format!("{}?{}", parts.uri.path(), kept.join("&"))
            };
            parts.uri = pq.parse().map_err(|_| bad_request("that path cannot be forwarded"))?;
        }
    }

    let ws = datalink::open(state, worker, row.id, StreamTarget::Port { port }).await?;
    let io = TokioIo::new(datalink::into_duplex(ws));
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, axum::body::Body>(io)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, format!("reaching the guest port: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let _ = mdb::touch(&state.pool, row.id).await;
    let mut resp = sender
        .send_request(axum::http::Request::from_parts(parts, body))
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, format!("the guest did not answer: {e}")))?;

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
            let guest_upgrade = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(client), Ok(guest)) = (client_upgrade.await, guest_upgrade.await) {
                    let mut client = TokioIo::new(client);
                    let mut guest = TokioIo::new(guest);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut guest).await;
                }
            });
        }
    }
    let (mut parts, body) = resp.into_parts();
    // A guest must not be able to set cookies on the control plane's origin.
    parts.headers.remove(axum::http::header::SET_COOKIE);
    Ok(Response::from_parts(parts, axum::body::Body::new(body)))
}

async fn data_ws(State(state): State<AppState>, ws: axum::extract::WebSocketUpgrade) -> Response {
    // Tarballs and screen streams: raise the frame ceiling from the default,
    // and never compress (the payloads are already compressed or binary).
    ws.max_message_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| datalink::handle_data_socket(state, socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_paths_must_be_absolute_and_contained() {
        assert!(guest_path("/home/pukubot/a.txt").is_ok());
        assert!(guest_path("relative").is_err());
        assert!(guest_path("/home/../etc/shadow").is_err());
        assert!(guest_path("/a\0b").is_err());
        // `..` as part of a name is just a name.
        assert!(guest_path("/home/a..b").is_ok());
    }

    #[test]
    fn archive_queries_collect_repeated_excludes() {
        let (path, ex, replace) =
            archive_query(Some("path=%2Fhome%2Fpukubot&exclude=.cache&exclude=node_modules&replace=1"));
        assert_eq!(path.as_deref(), Some("/home/pukubot"));
        assert_eq!(ex, vec![".cache".to_string(), "node_modules".to_string()]);
        assert!(replace);
        let (_, _, replace) = archive_query(Some("path=/x"));
        assert!(!replace);
    }

    #[test]
    fn percent_decoding_tolerates_junk() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn upgrade_detection_reads_the_connection_header() {
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("keep-alive, Upgrade"));
        assert!(is_upgrade(&h));
        h.insert("connection", HeaderValue::from_static("close"));
        assert!(!is_upgrade(&h));
    }
}
