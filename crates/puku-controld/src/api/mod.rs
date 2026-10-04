//! REST API + client attach WebSocket + session dispatch.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sentry::integrations::tower::{NewSentryLayer, SentryHttpLayer};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::client_ws::{ClientMsg, ServerMsg};
use puku_cloud_proto::event::EventKind;
use puku_cloud_proto::session::{ImportRef, PermissionMode, SessionSpec, SessionState};
use puku_cloud_proto::worker_proto::{ArtifactKind, Down, StopMode};
use puku_cloud_proto::Engine;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::db::{self, SessionRow};
use crate::workerlink::{accepts_input, send_to_worker, Placement};
use crate::AppState;

pub mod machines;

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/sessions", post(create_session).get(list_sessions))
        // A transcript is far larger than a normal request body; raise the
        // limit on this route alone rather than opening it globally.
        .route(
            "/v1/sessions/import",
            post(import_session).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/v1/sessions/{id}", get(get_session).delete(cancel_session))
        .route("/v1/sessions/{id}/events", get(get_events))
        .route("/v1/sessions/{id}/blobs/{line}", get(get_blob))
        .route("/v1/sessions/{id}/artifacts/{what}", post(collect_artifact).get(get_artifact))
        .route("/v1/sessions/{id}/answer", post(post_answer))
        .route("/v1/sessions/{id}/input", post(post_input))
        .route("/v1/sessions/{id}/interrupt", post(post_interrupt))
        .route("/v1/sessions/{id}/stop", post(post_stop))
        .route("/v1/sessions/{id}/resume", post(post_resume))
        .route("/v1/sessions/{id}/attach", get(attach))
        .route("/v1/schedules", post(create_schedule).get(list_schedules))
        .route("/v1/schedules/{id}", axum::routing::delete(delete_schedule))
        .route("/v1/schedules/{id}/enable", post(enable_schedule))
        .route("/v1/schedules/{id}/disable", post(disable_schedule))
        .route("/v1/schedules/{id}/run", post(run_schedule))
        .route("/v1/credentials", post(put_credential).get(list_credentials))
        .route("/v1/credentials/{id}", axum::routing::delete(delete_credential))
        .route("/v1/notifications", post(create_notification).get(list_notifications))
        .route("/v1/notifications/{id}", axum::routing::delete(delete_notification))
        .route("/v1/triggers", post(create_trigger).get(list_triggers))
        .route("/v1/triggers/{id}", axum::routing::delete(delete_trigger))
        .route("/v1/memory", get(get_memory_settings).post(set_memory_settings))
        .route("/v1/memory/profile", get(get_memory_profile))
        .route("/v1/fleet", get(fleet))
        .route("/v1/workers", get(list_workers))
        .route("/v1/workers/{id}/drain", post(drain_worker))
        .route("/v1/workers/{id}/undrain", post(undrain_worker))
        .merge(machines::protected())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::middleware,
        ));
    Router::new()
        // Workers authenticate with the worker token inside Register, not
        // with client API keys.
        .route("/v1/worker", get(worker_ws))
        // Unauthenticated so a load balancer, the Cloudflare tunnel and an
        // uptime check can all reach it. Exposes no tenant data.
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        // The hook URL's token IS its credential, so this route sits
        // outside the api-key middleware by design.
        .route("/v1/hooks/{token}", post(fire_hook))
        // The dashboard shell is public; every API call it makes carries
        // the user's key, so it renders nothing without one.
        .route("/", get(dashboard))
        // Capability links (the capability is the credential) and the worker
        // data plane (the worker token is, inside the first frame).
        .merge(machines::public())
        .merge(protected)
        // axum applies layers bottom-up, so the LAST .layer() is the
        // OUTERMOST. NewSentryLayer must therefore come last, binding a
        // fresh hub before SentryHttpLayer touches the scope -- reversed,
        // scope state bleeds between requests, which sentry-tower's own
        // docs describe as a memory leak.
        //
        // Both sit OUTSIDE the auth middleware on `protected`, so a 401 is
        // still recorded (as SpanStatus::Unauthenticated) and auth failures
        // keep their request context. Inside, every rejected request would
        // vanish.
        .layer(SentryHttpLayer::new().enable_transaction())
        .layer(NewSentryLayer::<axum::extract::Request>::new_from_top())
        .with_state(state)
}

#[derive(Deserialize, Default)]
struct HealthQuery {
    /// Round-trip an object to prove the storage credentials work. Off by
    /// default: the container HEALTHCHECK polls this endpoint every 30s and
    /// should not bill an R2 write each time.
    ///
    /// Kept as a string because serde's bool rejects `?deep=1`, and `1` is
    /// what anyone actually types into a health URL.
    deep: Option<String>,
}

impl HealthQuery {
    fn deep(&self) -> bool {
        matches!(self.deep.as_deref(), Some("1" | "true" | "yes" | ""))
    }
}

/// Liveness + readiness in one. Readiness is "can this instance actually
/// serve": the database round-trips. Worker count is reported but does not
/// fail the check — a control plane with no workers is still correctly
/// serving reads and queueing sessions.
///
/// `object_storage` reports only that a bucket is *configured*. Whether the
/// credentials work is a separate, paid question: ask for it with
/// `?deep=1`, which is what a deployment check should use. Reporting a
/// configured-but-unusable bucket as healthy sends operators looking in the
/// wrong place when a deliverable later 403s.
async fn health(State(state): State<AppState>, Query(q): Query<HealthQuery>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    let mut body = serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "database": if db_ok { "ok" } else { "unreachable" },
        "workers_connected": state.workers.len(),
        "object_storage": state.blobs.is_some(),
    });
    if q.deep() {
        let probe = match &state.blobs {
            None => serde_json::json!("not configured"),
            Some(b) => match b.probe().await {
                Ok(()) => serde_json::json!("ok"),
                Err(e) => serde_json::json!(format!("{e:#}")),
            },
        };
        body["object_storage_probe"] = probe;
    }
    let code = if db_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(body)).into_response()
}

/// Prometheus text exposition. Hand-rolled: the whole surface is a dozen
/// gauges, which is not worth a metrics framework and its registry.
async fn metrics(State(state): State<AppState>) -> Response {
    let mut out = String::new();
    let by_state: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, count(*) FROM sessions GROUP BY state",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    out.push_str("# HELP puku_sessions Sessions by state.\n");
    out.push_str("# TYPE puku_sessions gauge\n");
    for (st, n) in by_state {
        out.push_str(&format!("puku_sessions{{state=\"{st}\"}} {n}\n"));
    }

    let workers: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM workers GROUP BY status",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    out.push_str("# HELP puku_workers Registered workers by status.\n");
    out.push_str("# TYPE puku_workers gauge\n");
    for (st, n) in workers {
        out.push_str(&format!("puku_workers{{status=\"{st}\"}} {n}\n"));
    }

    // Registered-but-disconnected is the interesting failure: the row says
    // online while no socket is attached.
    out.push_str("# HELP puku_workers_connected Workers with a live control link to this instance.\n");
    out.push_str("# TYPE puku_workers_connected gauge\n");
    out.push_str(&format!("puku_workers_connected {}\n", state.workers.len()));

    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
        .into_response()
}

async fn dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/dashboard.html"
    )))
}

// ---------------------------------------------------------------- errors

struct AppError(StatusCode, String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Every 500 used to be serialised to the client and forgotten. There
        // was no tracing::error! anywhere on the HTTP path, so a handler that
        // failed left nothing behind at all -- not a log line, not a metric.
        // A whole week of debugging went into failures that had already
        // happened here and said nothing.
        //
        // 4xx stays quiet: those are the caller's own doing and reporting
        // them would bury the ones that are ours.
        if self.0.is_server_error() {
            tracing::error!(status = self.0.as_u16(), error = %self.1, "request failed");
        }
        (self.0, Json(serde_json::json!({"error": {"message": self.1}}))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        // `{e:#}` rather than `to_string()`: the anyhow context chain is the
        // useful half, and dropping it is why errors here read as "database
        // error" with no indication of which query.
        AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn not_found() -> AppError {
    AppError(StatusCode::NOT_FOUND, "session not found".into())
}

fn conflict(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::CONFLICT, msg.into())
}

fn forbidden() -> AppError {
    AppError(StatusCode::FORBIDDEN, "not allowed".into())
}

/// 409 helpers for the reliability-rebuild tier rules.    Per RSD §4.4:
/// premium sessions  must be warm; warm requires a  hot-pool worker; and warm
/// is only offered when the deployment has enough warm capacity to honour
/// it.   Returning a plain 409 leaves the caller guessing; these helpers give
/// the operator-actionable reason.
fn premium_requires_warm() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "premium SLA requires recovery_mode=warm_allowed; lower SLA or set recovery_mode=warm_allowed".into(),
    )
}

fn premium_requires_hot_pool() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "premium SLA requires a hot-pool worker; no worker currently advertises hot_pool".into(),
    )
}

fn warm_unavailable() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "warm recovery is not available on this deployment ( PUKU_OFFER_WARM=false or no warm capacity)".into(),
    )
}

type ApiResult<T> = Result<T, AppError>;

/// Load a session and enforce ownership.
///
/// Org membership alone is not enough: with per-user identity (M6) an org
/// can hold many users, and a session's transcript, its pending question
/// and its cancel button are the owner's, not the org's. Rows created
/// before user scoping have `user_id IS NULL` and stay org-visible.
fn owns(ctx: &AuthCtx, row_org: Uuid, row_user: Option<Uuid>) -> bool {
    if ctx.admin {
        return true;
    }
    if row_org != ctx.org_id {
        return false;
    }
    match (row_user, ctx.user_id) {
        (Some(owner), Some(caller)) => owner == caller,
        // Legacy rows with no owner remain visible to the org.
        (None, _) => true,
        // A caller with no user identity (a bare org api key) sees org rows.
        (Some(_), None) => true,
    }
}

async fn load_owned(state: &AppState, ctx: &AuthCtx, id: Uuid) -> ApiResult<SessionRow> {
    let row = db::get_session(&state.pool, id).await?.ok_or_else(not_found)?;
    if !owns(ctx, row.org_id, row.user_id) {
        return Err(not_found()); // don't leak existence across users or orgs
    }
    Ok(row)
}

/// Encrypt the caller's bearer for storage alongside the session.
///
/// Dispatch happens long after (and, for a resume, days after) the request
/// that created the session, so the credential has to be persisted rather
/// than held in the request. Returns None when the caller used a `pkc_` key
/// or when no `PUKU_SECRET_KEY` is configured — both fall back to the
/// operator's global credential.
fn encrypt_caller_credential(state: &AppState, ctx: &AuthCtx) -> Option<(String, Vec<u8>)> {
    let bearer = ctx.bearer.as_ref()?;
    let secrets = state.secrets.as_ref()?;
    match secrets.encrypt(bearer) {
        Ok(enc) => Some(("bearer".to_string(), enc)),
        Err(e) => {
            tracing::error!(error = %e, "encrypting the caller credential failed");
            None
        }
    }
}

// ---------------------------------------------------------------- sessions

#[derive(Deserialize)]
struct CreateSessionReq {
    prompt: String,
    /// Optional caller-supplied label; falls back to the prompt's first line.
    title: Option<String>,
    repo: Option<String>,
    branch: Option<String>,
    model: Option<String>,
    max_budget_usd: Option<f64>,
    /// Tool policy. An empty list takes the deployment's
    /// `PUKU_DEFAULT_{ALLOWED,DISALLOWED}_TOOLS`, which is empty by default
    /// and so restricts nothing. Naming any tool replaces that default
    /// outright -- there is no ceiling for tool lists, unlike
    /// `permission_mode`, which the deployment clamps.
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    permission_mode: Option<PermissionMode>,
    max_turns: Option<i32>,
    /// Attach the caller's brokered connectors (default true). Set false for
    /// a session that should reach nothing but its repo.
    connectors: Option<bool>,
    /// Skill packs, as `name` or `name@range`. Absent means the org's
    /// configured defaults in the registry.
    #[serde(default)]
    packs: Vec<String>,
    /// JSON Schema the final answer must satisfy. For runs a program
    /// consumes: a scheduled job returning a verdict instead of prose.
    output_schema: Option<serde_json::Value>,
    /// Keep this session out of memory entirely — neither read nor written.
    /// For a run that handles secrets or reads untrusted input, where a
    /// remembered fact would be a liability rather than an asset.
    #[serde(default)]
    memory_opt_out: bool,
    idle_timeout_s: Option<i32>,
    max_duration_s: Option<i32>,
    /// `libkrun` or `cloud_hypervisor`. Absent takes PUKU_ENGINE_DEFAULT,
    /// which is what every request before this field got.
    engine: Option<Engine>,
}

/// The engine a request runs on, or a 400 naming what this deployment offers.
fn resolve_engine(state: &AppState, requested: Option<Engine>) -> ApiResult<Engine> {
    state
        .cfg
        .resolve_engine(requested)
        .map_err(|reason| AppError(StatusCode::BAD_REQUEST, reason))
}

async fn create_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<CreateSessionReq>,
) -> ApiResult<Json<SessionRow>> {
    let engine = resolve_engine(&state, req.engine)?;
    if let Err(reason) = db::check_quota(&state.pool, ctx.org_id).await? {
        return Err(AppError(StatusCode::TOO_MANY_REQUESTS, reason));
    }
    // Clamp here, at the edge, so the stored row is already the effective
    // policy — no caller-supplied privilege survives into the database.
    let permission_mode = req
        .permission_mode
        .map(|m| m.clamp_to(state.cfg.permission_ceiling))
        .unwrap_or(state.cfg.permission_ceiling);
    let row = db::create_session(
        &state.pool,
        db::NewSession {
            org_id: ctx.org_id,
            user_id: ctx.user_id,
            title: req.title,
            prompt: req.prompt,
            repo: req.repo,
            branch: req.branch,
            model: req.model,
            max_budget_usd: req.max_budget_usd,
            allowed_tools: req.allowed_tools,
            disallowed_tools: req.disallowed_tools,
            permission_mode: Some(permission_mode.as_cli_str().to_string()),
            max_turns: req.max_turns,
            // Run on the caller's own credential when they authenticated
            // with a platform bearer. Falls back to the operator's global
            // key, which is what every session used before M6.
            credential: encrypt_caller_credential(&state, &ctx),
            connectors: req.connectors,
            packs: req.packs,
            output_schema: req.output_schema,
            idle_timeout_s: req.idle_timeout_s,
            max_duration_s: req.max_duration_s,
            memory_opt_out: req.memory_opt_out,
            // Not a teleport: no transcript, and the id is generated.
            id: None,
            puku_session_id: None,
            import_ref: None,
            import_inline: None,
            engine,
        },
    )
    .await?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.create", &row.id.to_string(), serde_json::json!({}))
        .await
        .ok();
    if let Err(e) = dispatch_pending(&state).await {
        tracing::warn!(error = %e, "dispatch after create failed");
    }
    note_if_unplaceable(&state, row.id).await;
    let row = db::get_session(&state.pool, row.id).await?.ok_or_else(not_found)?;
    Ok(Json(row))
}

/// Say so, once, when no connected worker can run what the caller asked for.
///
/// Otherwise a session for an engine nobody is running sits `created` with
/// nothing on its transcript to explain why. Emitted at the edge rather than
/// by the dispatcher, which runs every few seconds and would repeat it.
async fn note_if_unplaceable(state: &AppState, session_id: Uuid) {
    let Ok(Some(s)) = db::get_session(&state.pool, session_id).await else { return };
    if s.worker_id.is_some() || s.session_state() != SessionState::Created {
        return;
    }
    let engine = s.session_engine();
    if state.workers.any_runs(engine) {
        return; // busy, not absent: it gets the next free slot
    }
    let message = format!(
        "no connected worker runs the {engine} engine; the session will start when one does"
    );
    if let Ok(ev) = db::insert_platform_event(
        &state.pool,
        session_id,
        EventKind::Session,
        serde_json::json!({
            "type": "session.waiting_for_worker",
            "engine": engine.as_str(),
            "message": message,
        }),
    )
    .await
    {
        state.publish_events(&[ev]).await;
    }
}

#[derive(Deserialize)]
struct ImportSessionReq {
    /// puku-cli's own session id for the local conversation. The cloud
    /// resumes *this* id, so it must match the transcript's filename.
    puku_session_id: String,
    /// The local transcript, JSONL exactly as puku-cli wrote it.
    transcript: String,
    /// Keep this session out of memory entirely.
    #[serde(default)]
    memory_opt_out: bool,
    /// Where to pick the conversation back up.
    prompt: Option<String>,
    repo: Option<String>,
    branch: Option<String>,
    model: Option<String>,
    title: Option<String>,
    max_budget_usd: Option<f64>,
    max_turns: Option<i32>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    permission_mode: Option<PermissionMode>,
    connectors: Option<bool>,
    idle_timeout_s: Option<i32>,
    max_duration_s: Option<i32>,
    engine: Option<Engine>,
}

/// Continue a local puku-cli session in the cloud.
///
/// The session is created already-resumed: `puku_session_id` is set, so
/// `build_spec` marks `resume: true` and the runner plants the transcript
/// under the project directory the guest's own cwd resolves to before
/// running `--resume`.
async fn import_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<ImportSessionReq>,
) -> ApiResult<Json<SessionRow>> {
    let engine = resolve_engine(&state, req.engine)?;
    if let Err(reason) = db::check_quota(&state.pool, ctx.org_id).await? {
        return Err(AppError(StatusCode::TOO_MANY_REQUESTS, reason));
    }
    if req.transcript.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "transcript is empty — nothing to continue".into(),
        ));
    }
    // Refuse a transcript we cannot deliver, rather than starting a session
    // that looks resumed and has no memory of anything.
    if state.blobs.is_none()
        && req.transcript.len() > puku_cloud_proto::session::MAX_INLINE_IMPORT_BYTES
    {
        return Err(AppError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "transcript is {} bytes; this deployment has no object storage, so imports are                  capped at {} bytes. Configure PUKU_R2_* to lift the cap.",
                req.transcript.len(),
                puku_cloud_proto::session::MAX_INLINE_IMPORT_BYTES
            ),
        ));
    }

    let permission_mode = req
        .permission_mode
        .map(|m| m.clamp_to(state.cfg.permission_ceiling))
        .unwrap_or(state.cfg.permission_ceiling);
    // The id is chosen here rather than inside `create_session` because the
    // transcript's object key derives from it, and the transcript has to be
    // stored *before* the row exists -- otherwise the row is dispatchable
    // before its history is reachable.
    let session_id = Uuid::new_v4();
    let (import_ref, import_inline) = store_import(&state, session_id, &req.transcript).await?;
    let row = db::create_session(
        &state.pool,
        db::NewSession {
            org_id: ctx.org_id,
            user_id: ctx.user_id,
            title: req.title,
            // A resumed session's "prompt" is the follow-up turn, if any.
            prompt: req.prompt.unwrap_or_default(),
            memory_opt_out: req.memory_opt_out,
            repo: req.repo,
            branch: req.branch,
            model: req.model,
            max_budget_usd: req.max_budget_usd,
            allowed_tools: req.allowed_tools,
            disallowed_tools: req.disallowed_tools,
            permission_mode: Some(permission_mode.as_cli_str().to_string()),
            max_turns: req.max_turns,
            credential: encrypt_caller_credential(&state, &ctx),
            connectors: req.connectors,
            // A teleported session continues a local one, so it inherits
            // the org's default packs rather than naming its own.
            packs: Vec::new(),
            output_schema: None,
            idle_timeout_s: req.idle_timeout_s,
            max_duration_s: req.max_duration_s,
            // Built complete, in one INSERT. A row lands in `created` and
            // is dispatchable that instant; writing the transcript in a
            // follow-up UPDATE left a window where the dispatcher sent a
            // spec with `resume: false` and no history, and the teleported
            // conversation was silently gone.
            id: Some(session_id),
            puku_session_id: Some(req.puku_session_id.clone()),
            import_ref,
            import_inline,
            engine,
        },
    )
    .await?;

    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.import", &row.id.to_string(), serde_json::json!({"bytes": req.transcript.len()}))
        .await
        .ok();
    if let Err(e) = dispatch_pending(&state).await {
        tracing::warn!(error = %e, "dispatch after import failed");
    }
    note_if_unplaceable(&state, row.id).await;
    Ok(Json(db::get_session(&state.pool, row.id).await?.ok_or_else(not_found)?))
}

/// Put the transcript where the worker can get it: object storage when
/// configured, otherwise inline. Returns `(object_ref, inline_bytes)` —
/// exactly one is populated.
async fn store_import(
    state: &AppState,
    session_id: Uuid,
    transcript: &str,
) -> ApiResult<(Option<String>, Option<String>)> {
    match &state.blobs {
        Some(blobs) => {
            let key = crate::blobstore::BlobStore::import_key(session_id);
            blobs
                .put(&key, transcript.as_bytes().to_vec(), "application/x-ndjson")
                .await
                .map_err(|e| {
                    AppError(StatusCode::BAD_GATEWAY, format!("storing the transcript failed: {e:#}"))
                })?;
            Ok((Some(blobs.blob_ref(&key)), None))
        }
        None => Ok((None, Some(transcript.to_string()))),
    }
}

/// Resolve a session's stored transcript into the carrier the worker reads.
/// Presigned at dispatch, not at import, so the URL is fresh when used.
async fn resolve_import(state: &AppState, session_id: Uuid) -> Option<ImportRef> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT import_ref, import_inline FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();
    let (object_ref, inline) = row?;
    if let Some(jsonl) = inline {
        return Some(ImportRef::Inline { jsonl });
    }
    let object_ref = object_ref?;
    let blobs = state.blobs.as_ref()?;
    let key = blobs.key_from_ref(&object_ref)?;
    Some(ImportRef::Url { url: blobs.presign_get(&key) })
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    limit: Option<i64>,
    /// A PR number or URL. Resolves the session that opened it, which is
    /// what `puku cloud attach --from-pr` needs: the mapping already exists
    /// on `sessions.pr_url`.
    pr: Option<String>,
    /// `scope=org` widens the listing from the caller's own sessions to the
    /// whole org. Available to org api keys and admins.
    scope: Option<String>,
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<SessionRow>>> {
    // Default to the caller's own sessions; `scope=org` opts into the
    // whole org. A bare org api key has no user identity and sees the org.
    let user_filter = match q.scope.as_deref() {
        Some("org") => None,
        _ => ctx.user_id,
    };
    if let Some(pr) = q.pr.as_deref() {
        return Ok(Json(db::sessions_for_pr(&state.pool, ctx.org_id, user_filter, pr).await?));
    }
    let rows = db::list_sessions(
        &state.pool,
        ctx.org_id,
        user_filter,
        q.state.as_deref(),
        q.limit.unwrap_or(50).min(200),
    )
    .await?;
    Ok(Json(rows))
}

async fn get_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SessionRow>> {
    Ok(Json(load_owned(&state, &ctx, id).await?))
}

#[derive(Deserialize)]
struct EventsQuery {
    after_seq: Option<i64>,
    limit: Option<i64>,
}

async fn get_events(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Vec<puku_cloud_proto::event::Event>>> {
    load_owned(&state, &ctx, id).await?;
    let events = db::fetch_events(
        &state.pool,
        id,
        q.after_seq.unwrap_or(0),
        q.limit.unwrap_or(500).min(2000),
    )
    .await?;
    Ok(Json(events))
}

/// Redirect to a presigned GET for an event's spilled payload.
///
/// Oversized event lines are truncated in the transcript and the full
/// payload is uploaded to object storage under a key derived from
/// (session, guest line). Streaming the bytes through controld would put
/// arbitrarily large tool outputs on the control plane's heap, so hand the
/// client a short-lived URL and let it fetch directly.
async fn get_blob(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path((id, line)): Path<(Uuid, i64)>,
) -> ApiResult<Response> {
    load_owned(&state, &ctx, id).await?;
    let blobs = state
        .blobs
        .as_ref()
        .ok_or_else(|| AppError(StatusCode::NOT_IMPLEMENTED, "object storage not configured".into()))?;
    // Only serve a blob the transcript actually references, so the endpoint
    // can't be used to probe the bucket.
    let key = crate::blobstore::BlobStore::blob_key(id, line);
    let referenced: Option<(String,)> = sqlx::query_as(
        "SELECT blob_ref FROM session_events \
         WHERE session_id = $1 AND guest_line = $2 AND blob_ref IS NOT NULL LIMIT 1",
    )
    .bind(id)
    .bind(line)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some((blob_ref,)) = referenced else {
        return Err(AppError(StatusCode::NOT_FOUND, "no blob for that event".into()));
    };
    // Pre-R2 sessions recorded `session://blobs/…`, a worker-local path on a
    // volume that has since been reaped. Say so instead of 404ing blankly.
    if blob_ref.starts_with("session://") {
        return Err(AppError(
            StatusCode::GONE,
            "this event predates object storage; its payload was on a reaped volume".into(),
        ));
    }
    let url = blobs.presign_get(&key);
    Ok((StatusCode::FOUND, [(axum::http::header::LOCATION, url)]).into_response())
}

// ----------------------------------------------------------- deliverables

/// Ask the worker to package `/workspace` or `/session/home` and upload it.
///
/// Work that never leaves the VM didn't happen: the volumes are reaped, so
/// without this a user gets a transcript and nothing else. `home` is also
/// what teleport-down needs — it is puku-cli's own transcript, so a local
/// `puku-cli --resume` can pick the conversation up off-cloud.
async fn collect_artifact(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path((id, what)): Path<(Uuid, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let session = load_owned(&state, &ctx, id).await?;
    let kind = ArtifactKind::parse(&what)
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "what must be workspace or home".into()))?;
    if state.blobs.is_none() {
        return Err(AppError(
            StatusCode::NOT_IMPLEMENTED,
            "object storage is not configured; artifacts cannot be collected".into(),
        ));
    }
    // The volumes only exist while a worker owns the session. Once it is
    // reaped there is nothing left to package, and saying so beats a
    // request that quietly never completes.
    if session.worker_id.is_none() || session.session_state() == SessionState::Reaped {
        return Err(conflict(
            "session has no live volumes; artifacts must be collected before it is reaped",
        ));
    }
    let key = crate::blobstore::BlobStore::artifact_key(id, kind.as_str());
    send_to_worker(&state, session.worker_id, Down::CollectArtifact {
        session_id: id,
        what: kind,
        key: key.clone(),
    })
    .map_err(|e| conflict(e.to_string()))?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.artifact", &id.to_string(), serde_json::json!({"what": what}))
        .await
        .ok();
    // Asynchronous by nature: taring a large workspace takes a while, and
    // the transcript carries `session.artifact_ready` when it lands.
    Ok(Json(serde_json::json!({
        "status": "collecting",
        "what": kind.as_str(),
        "download": format!("/v1/sessions/{id}/artifacts/{}", kind.as_str()),
    })))
}

/// Redirect to a presigned GET for a previously collected artifact.
async fn get_artifact(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path((id, what)): Path<(Uuid, String)>,
) -> ApiResult<Response> {
    load_owned(&state, &ctx, id).await?;
    let kind = ArtifactKind::parse(&what)
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "what must be workspace or home".into()))?;
    let blobs = state
        .blobs
        .as_ref()
        .ok_or_else(|| AppError(StatusCode::NOT_IMPLEMENTED, "object storage not configured".into()))?;
    // Only redirect once the worker has actually reported the upload, or
    // the caller follows a URL to a 404 they can't interpret.
    let ready: Option<(i64,)> = sqlx::query_as(
        "SELECT seq FROM session_events \
         WHERE session_id = $1 AND payload->>'type' = 'session.artifact_ready' \
           AND payload->>'what' = $2 ORDER BY seq DESC LIMIT 1",
    )
    .bind(id)
    .bind(kind.as_str())
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if ready.is_none() {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no {} artifact yet; POST this path to collect one", kind.as_str()),
        ));
    }
    let url = blobs.presign_get(&crate::blobstore::BlobStore::artifact_key(id, kind.as_str()));
    Ok((StatusCode::FOUND, [(axum::http::header::LOCATION, url)]).into_response())
}

// ------------------------------------------------------------- input paths

/// Wrap user text as a puku-cli stream-json user message and deliver it to
/// the session's stdin fifo via the worker. Echoes a `user` event.
///
/// This is the follow-up-turn path only. A *pending question* is a blocked
/// `control_request` and cannot be cleared by a user message — see
/// `deliver_answer` / `build_control_response`.
async fn deliver_user_text(
    state: &AppState,
    session: &SessionRow,
    text: &str,
) -> anyhow::Result<()> {
    if !accepts_input(session.session_state()) {
        anyhow::bail!("session is {} — not accepting input", session.state);
    }
    let line = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
    .to_string();
    send_to_worker(
        state,
        session.worker_id,
        Down::DeliverInput { session_id: session.id, stream_json_line: line },
    )?;
    let ev = db::insert_platform_event(
        &state.pool,
        session.id,
        EventKind::User,
        serde_json::json!({"type": "user.input", "text": text}),
    )
    .await?;
    state.publish_events(&[ev]).await;
    Ok(())
}

#[derive(Deserialize)]
struct AnswerReq {
    /// The `request_id` from `pending_question`.
    question_id: String,
    /// The human's answer. Applied to every question in the ask, which is
    /// the common single-question case.
    #[serde(default)]
    answer: Option<String>,
    /// Explicit per-question answers, keyed by the question's `header`.
    /// Use this when the ask carries more than one question.
    #[serde(default)]
    answers: Option<std::collections::HashMap<String, String>>,
    /// `deny` refuses the tool instead of answering it. The agent sees an
    /// errored tool_result and routes around it.
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Build the stream-json line that satisfies a pending `can_use_tool`
/// request.
///
/// This is a **control_response**, not a user message. Verified against
/// puku-cli 1.8.43: the CLI blocks on the control channel until this exact
/// frame arrives (it waits indefinitely — measured past 75 s), and a user
/// message does not clear it. The human's answer rides in
/// `updatedInput.answers`, a map keyed by each question's `header`
/// (puku-cli falls back to `Q1`, `Q2`, … when a question has none), which
/// the CLI renders back to the model as `"<header>"="<answer>"`.
fn build_control_response(
    pending: &serde_json::Value,
    req: &AnswerReq,
) -> anyhow::Result<String> {
    let request_id = pending
        .get("request_id")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow::anyhow!("pending question has no request_id"))?;
    if request_id != req.question_id {
        anyhow::bail!(
            "question {} is no longer pending (current: {request_id})",
            req.question_id
        );
    }

    // The SDK runner owns the CLI's control channel through `canUseTool`, so
    // it cannot be handed a control_response — it is answered with a plain
    // line and does the PermissionResult construction itself, in the process
    // that actually holds the question object.
    //
    // That is the whole point of the dialect split: the header/`Q{n}`
    // answer-keying below exists only because this code has to rebuild
    // `updatedInput` blind, from a projection, in a different process.
    if pending.get("kind").and_then(|k| k.as_str()) == Some("platform.question") {
        let deny = req.decision.as_deref() == Some("deny");
        // Same rule as the CLI dialect, and it has to be enforced here too.
        // The runner does refuse an empty allow, but by then controld has
        // already cleared pending_question and moved the session on -- so the
        // caller's *real* answer comes back 409 and the question is
        // unanswerable. An allow with nothing in it must fail at the edge.
        if !deny
            && req.answer.is_none()
            && req.answers.as_ref().is_none_or(|m| m.is_empty())
            && pending
                .get("input")
                .and_then(|i| i.get("questions"))
                .and_then(|q| q.as_array())
                .is_some_and(|q| !q.is_empty())
        {
            anyhow::bail!("answer required: this question expects one");
        }
        let mut line = serde_json::json!({
            "type": "platform.answer",
            "request_id": request_id,
            "decision": if deny { "deny" } else { "allow" },
        });
        if let Some(m) = &req.message {
            line["message"] = serde_json::Value::String(m.clone());
        }
        if let Some(a) = &req.answer {
            line["answer"] = serde_json::Value::String(a.clone());
        }
        if let Some(map) = &req.answers {
            line["answers"] = serde_json::json!(map);
        }
        return Ok(line.to_string());
    }

    let response = if req.decision.as_deref() == Some("deny") {
        serde_json::json!({
            "behavior": "deny",
            "message": req.message.clone().unwrap_or_else(|| "declined by the user".into()),
        })
    } else {
        let input = pending.get("input").cloned().unwrap_or_else(|| serde_json::json!({}));
        let mut updated = input.clone();
        let questions = input.get("questions").and_then(|q| q.as_array());
        // Only AskUserQuestion-shaped asks carry questions to answer; a bare
        // permission ask ("may I run Bash?") is satisfied by allow alone.
        if let Some(questions) = questions {
            let mut answers = serde_json::Map::new();
            for (i, q) in questions.iter().enumerate() {
                let header = q
                    .get("header")
                    .and_then(|h| h.as_str())
                    .filter(|h| !h.is_empty())
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| format!("Q{}", i + 1));
                let value = req
                    .answers
                    .as_ref()
                    .and_then(|m| m.get(&header))
                    .or(req.answer.as_ref());
                if let Some(value) = value {
                    answers.insert(header, serde_json::Value::String(value.clone()));
                }
            }
            if answers.is_empty() {
                anyhow::bail!("answer required: this question expects one");
            }
            updated["answers"] = serde_json::Value::Object(answers);
        }
        serde_json::json!({"behavior": "allow", "updatedInput": updated})
    };

    Ok(serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
    .to_string())
}

async fn post_answer(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Json(req): Json<AnswerReq>,
) -> ApiResult<StatusCode> {
    let session = load_owned(&state, &ctx, id).await?;
    deliver_answer(&state, &session, &req)
        .await
        .map_err(|e| conflict(e.to_string()))?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.answer", &id.to_string(), serde_json::json!({}))
        .await
        .ok();
    Ok(StatusCode::ACCEPTED)
}

/// Answer the session's pending question and unblock it.
async fn deliver_answer(
    state: &AppState,
    session: &SessionRow,
    req: &AnswerReq,
) -> anyhow::Result<()> {
    let Some(pending) = session.pending_question.as_ref() else {
        anyhow::bail!("session has no pending question");
    };
    let line = build_control_response(pending, req)?;
    send_to_worker(
        state,
        session.worker_id,
        Down::DeliverInput { session_id: session.id, stream_json_line: line },
    )?;

    let echo = req
        .answer
        .clone()
        .or_else(|| {
            req.answers.as_ref().map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        })
        .unwrap_or_else(|| "declined".into());
    let ev = db::insert_platform_event(
        &state.pool,
        session.id,
        EventKind::User,
        serde_json::json!({
            "type": "user.answer",
            "question_id": req.question_id,
            "decision": req.decision.clone().unwrap_or_else(|| "allow".into()),
            "text": echo,
        }),
    )
    .await?;
    state.publish_events(&[ev]).await;

    sqlx::query("UPDATE sessions SET pending_question = NULL WHERE id = $1")
        .bind(session.id)
        .execute(&state.pool)
        .await?;
    if session.session_state() == SessionState::WaitingInput {
        if let (_, Some(ev)) =
            db::transition(&state.pool, session.id, SessionState::Running, None).await?
        {
            state.publish_events(&[ev]).await;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct InputReq {
    text: String,
}

async fn post_input(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    Json(req): Json<InputReq>,
) -> ApiResult<StatusCode> {
    let session = load_owned(&state, &ctx, id).await?;
    // A finished session keeps its volumes, so a follow-up turn is a resume,
    // not an error. This is the ordinary way to continue a conversation now
    // that a successful result completes the session instead of leaving it
    // running until the idle timeout.
    if resumable(session.session_state()) {
        // The runner delivers `prompt` whenever it is non-empty, resume or
        // not, so replacing it is how the next turn reaches the agent -- and
        // it stops a resume from re-running the original instruction. It
        // rides the transition's transaction so a refused resume cannot
        // leave the session holding a prompt it never ran.
        return resume_session(&state, &ctx, id, Some(req.text.as_str())).await;
    }
    deliver_user_text(&state, &session, &req.text)
        .await
        .map_err(|e| conflict(e.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

/// States whose volumes still exist, so the session can be booted again.
/// `canceled` and `reaped` have had their volumes discarded.
fn resumable(state: SessionState) -> bool {
    matches!(state, SessionState::Stopped | SessionState::Completed | SessionState::Failed)
}

async fn post_interrupt(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let session = load_owned(&state, &ctx, id).await?;
    send_to_worker(&state, session.worker_id, Down::Interrupt { session_id: id })
        .map_err(|e| conflict(e.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

// -------------------------------------------------------- lifecycle verbs

async fn post_stop(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let session = load_owned(&state, &ctx, id).await?;
    if !accepts_input(session.session_state()) {
        return Err(conflict(format!("session is {}", session.state)));
    }
    let (_, ev) = db::transition(&state.pool, id, SessionState::Stopping, None)
        .await
        .map_err(|e| conflict(e.to_string()))?;
    if let Some(ev) = ev {
        state.publish_events(&[ev]).await;
    }
    send_to_worker(&state, session.worker_id, Down::StopSession { session_id: id, mode: StopMode::Park })
        .map_err(|e| conflict(e.to_string()))?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.stop", &id.to_string(), serde_json::json!({}))
        .await
        .ok();
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let session = load_owned(&state, &ctx, id).await?;
    let st = session.session_state();
    if st.is_terminal() {
        return Ok(StatusCode::NO_CONTENT);
    }
    // Best effort: tell the worker to kill; the authoritative transition
    // comes back as a SessionState frame, but mark canceled for sessions
    // that never reached a worker.
    if session.worker_id.is_some() {
        let _ = send_to_worker(&state, session.worker_id, Down::StopSession {
            session_id: id,
            mode: StopMode::Kill,
        });
    }
    if let Ok((_, Some(ev))) = db::transition(&state.pool, id, SessionState::Canceled, None).await {
        state.publish_events(&[ev]).await;
    }
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.cancel", &id.to_string(), serde_json::json!({}))
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

async fn post_resume(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    resume_session(&state, &ctx, id, None).await
}

/// Boot a finished or parked session again on its existing volumes,
/// optionally with a new prompt for the turn it is about to run.
async fn resume_session(
    state: &AppState,
    ctx: &AuthCtx,
    id: Uuid,
    prompt: Option<&str>,
) -> ApiResult<StatusCode> {
    let session = load_owned(state, ctx, id).await?;
    if !resumable(session.session_state()) {
        return Err(conflict(format!("session is {}, which cannot resume", session.state)));
    }
    // Re-capture the caller's credential. A platform bearer lives ~6h; a
    // session parked overnight and resumed tomorrow would otherwise come
    // back with a dead token and 401 on its first model call. The resuming
    // request carries a fresh one, so use it.
    if let Some(cred) = encrypt_caller_credential(state, ctx) {
        db::set_session_credential(&state.pool, id, Some(cred))
            .await
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    // Clear the worker so the dispatcher re-places it. It re-places it on
    // `volume_worker_id` and nowhere else: the workspace is on that host's
    // disk, and a resume anywhere else boots `--resume` against nothing.
    sqlx::query("UPDATE sessions SET worker_id = NULL WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (_, ev) = db::transition_with_prompt(&state.pool, id, SessionState::Scheduled, None, prompt)
        .await
        .map_err(|e| conflict(e.to_string()))?;
    if let Some(ev) = ev {
        state.publish_events(&[ev]).await;
    }
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "session.resume", &id.to_string(), serde_json::json!({}))
        .await
        .ok();
    dispatch_pending(state).await?;
    Ok(StatusCode::ACCEPTED)
}

// ------------------------------------------------------------- schedules

#[derive(Deserialize)]
struct CreateScheduleReq {
    name: Option<String>,
    prompt: String,
    cron: String,
    repo: Option<String>,
    branch: Option<String>,
    model: Option<String>,
    max_budget_usd: Option<f64>,
    /// Same policy surface as `POST /v1/sessions`, so a scheduled task is
    /// no less configurable than the interactive run it was built from.
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    permission_mode: Option<PermissionMode>,
    max_turns: Option<i32>,
    connectors: Option<bool>,
    /// Skill packs for every run this schedule fires. Empty falls through
    /// to the org's defaults, matching `POST /v1/sessions`.
    #[serde(default)]
    packs: Vec<String>,
    /// Keep every run this schedule fires out of memory.
    #[serde(default)]
    memory_opt_out: bool,
    idle_timeout_s: Option<i32>,
    engine: Option<Engine>,
}

async fn create_schedule(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<CreateScheduleReq>,
) -> ApiResult<Json<crate::scheduler::ScheduleRow>> {
    let engine = resolve_engine(&state, req.engine)?;
    let row = crate::scheduler::create(&state.pool, crate::scheduler::NewSchedule {
        engine,
        memory_opt_out: req.memory_opt_out,
        org_id: ctx.org_id,
        user_id: ctx.user_id,
        name: req.name.unwrap_or_default(),
        prompt: req.prompt,
        repo: req.repo,
        branch: req.branch,
        model: req.model,
        max_budget_usd: req.max_budget_usd,
        allowed_tools: req.allowed_tools,
        disallowed_tools: req.disallowed_tools,
        // Clamp at the edge, like create_session does, so the stored row is
        // already the effective policy.
        permission_mode: req
            .permission_mode
            .map(|m| m.clamp_to(state.cfg.permission_ceiling).as_cli_str().to_string()),
        max_turns: req.max_turns,
        connectors: req.connectors,
        packs: req.packs,
        idle_timeout_s: req.idle_timeout_s,
        cron: req.cron,
    })
    .await
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "schedule.create", &row.id.to_string(), serde_json::json!({"cron": row.cron}))
        .await
        .ok();
    Ok(Json(row))
}

async fn list_schedules(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<Vec<crate::scheduler::ScheduleRow>>> {
    Ok(Json(crate::scheduler::list(&state.pool, ctx.org_id).await?))
}

async fn load_schedule(
    state: &AppState,
    ctx: &AuthCtx,
    id: Uuid,
) -> ApiResult<crate::scheduler::ScheduleRow> {
    let row = crate::scheduler::get(&state.pool, id)
        .await?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "schedule not found".into()))?;
    if !owns(ctx, row.org_id, row.user_id) {
        return Err(AppError(StatusCode::NOT_FOUND, "schedule not found".into()));
    }
    Ok(row)
}

async fn delete_schedule(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    load_schedule(&state, &ctx, id).await?;
    crate::scheduler::remove(&state.pool, id).await?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "schedule.delete", &id.to_string(), serde_json::json!({}))
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

async fn enable_schedule(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    load_schedule(&state, &ctx, id).await?;
    crate::scheduler::set_enabled(&state.pool, id, true).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn disable_schedule(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    load_schedule(&state, &ctx, id).await?;
    crate::scheduler::set_enabled(&state.pool, id, false).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Fire the schedule immediately (does not shift its cron cadence).
async fn run_schedule(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let sched = load_schedule(&state, &ctx, id).await?;
    let session_id = crate::scheduler::fire(&state, &sched)
        .await
        .map_err(|e| conflict(e.to_string()))?;
    dispatch_pending(&state).await?;
    Ok(Json(serde_json::json!({"session_id": session_id})))
}

// ----------------------------------------------------------- credentials

#[derive(Deserialize)]
struct PutCredentialReq {
    /// `api_key` — a puku platform key with no expiry, which is what an
    /// unattended schedule actually wants. `bearer` is accepted for
    /// completeness but expires in hours.
    kind: String,
    value: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Store the credential unattended runs authenticate with.
///
/// Without this a scheduled session has no caller to borrow a bearer from
/// and falls through to the operator's global key — so every org's cron
/// spend lands on the operator's account.
async fn put_credential(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<PutCredentialReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if !matches!(req.kind.as_str(), "api_key" | "bearer" | "refresh") {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "kind must be api_key, bearer or refresh. Prefer refresh: it is the only one \
             that keeps unattended runs working past the life of a login."
                .into(),
        ));
    }
    if req.value.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "value must not be empty".into()));
    }
    let secrets = state.secrets.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::NOT_IMPLEMENTED,
            "this deployment has no PUKU_SECRET_KEY, so credentials cannot be stored encrypted".into(),
        )
    })?;
    let enc = secrets
        .encrypt(&req.value)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let id = db::put_org_credential(
        &state.pool,
        ctx.org_id,
        ctx.user_id,
        &req.kind,
        enc,
        req.expires_at,
    )
    .await?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "credential.put", &id.to_string(), serde_json::json!({"kind": req.kind}))
        .await
        .ok();
    Ok(Json(serde_json::json!({"id": id, "kind": req.kind})))
}

async fn list_credentials(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let rows = db::list_org_credentials(&state.pool, ctx.org_id).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, kind, expires_at, created_at)| {
                serde_json::json!({
                    "id": id, "kind": kind, "expires_at": expires_at, "created_at": created_at,
                })
            })
            .collect(),
    ))
}

async fn delete_credential(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !db::delete_org_credential(&state.pool, ctx.org_id, id).await? {
        return Err(AppError(StatusCode::NOT_FOUND, "credential not found".into()));
    }
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "credential.delete", &id.to_string(), serde_json::json!({}))
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------- notifications

#[derive(Deserialize)]
struct CreateNotificationReq {
    /// `webhook` | `slack` | `platform`
    kind: String,
    /// Destination for webhook/slack.
    url: Option<String>,
    /// Which triggers to deliver; defaults to both.
    #[serde(default)]
    events: Vec<String>,
    /// HMAC key for webhook signatures. Stored encrypted.
    secret: Option<String>,
}

async fn create_notification(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<CreateNotificationReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if !matches!(req.kind.as_str(), "webhook" | "slack" | "platform") {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "kind must be webhook, slack or platform".into(),
        ));
    }
    if matches!(req.kind.as_str(), "webhook" | "slack") && req.url.is_none() {
        return Err(AppError(StatusCode::BAD_REQUEST, format!("{} needs a url", req.kind)));
    }
    let events = if req.events.is_empty() {
        vec!["waiting_input".to_string(), "terminal".to_string()]
    } else {
        for e in &req.events {
            if !matches!(e.as_str(), "waiting_input" | "terminal") {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("unknown event {e:?}; expected waiting_input or terminal"),
                ));
            }
        }
        req.events.clone()
    };
    let secret_enc = match (&req.secret, &state.secrets) {
        (Some(secret), Some(box_)) => Some(
            box_.encrypt(secret)
                .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        ),
        (Some(_), None) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "a signing secret needs PUKU_SECRET_KEY configured on the server".into(),
            ))
        }
        (None, _) => None,
    };
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO notification_targets (id, org_id, user_id, kind, config, events, secret_enc)          VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(id)
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .bind(&req.kind)
    .bind(serde_json::json!({"url": req.url}))
    .bind(&events)
    .bind(&secret_enc)
    .execute(&state.pool)
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"id": id, "kind": req.kind, "events": events})))
}

async fn list_notifications(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    // Never return config verbatim — it holds the Slack webhook URL, which
    // is itself a credential.
    type Row = (Uuid, String, Vec<String>, bool, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, kind, events, enabled, config->>'url' FROM notification_targets \
         WHERE org_id = $1 AND (user_id IS NULL OR user_id IS NOT DISTINCT FROM $2) \
         ORDER BY created_at",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, kind, events, enabled, url)| {
                serde_json::json!({
                    "id": id, "kind": kind, "events": events, "enabled": enabled,
                    "url": url.as_deref().map(redact_url),
                })
            })
            .collect(),
    ))
}

/// Show enough of a destination to recognise it, not enough to reuse it.
fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}/…")
        }
        None => "…".to_string(),
    }
}

async fn delete_notification(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let n = sqlx::query(
        "DELETE FROM notification_targets WHERE id = $1 AND org_id = $2 \
         AND (user_id IS NULL OR user_id IS NOT DISTINCT FROM $3)",
    )
    .bind(id)
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .execute(&state.pool)
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if n.rows_affected() == 0 {
        return Err(AppError(StatusCode::NOT_FOUND, "notification target not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------- triggers

#[derive(Deserialize)]
struct CreateTriggerReq {
    prompt: String,
    #[serde(default)]
    name: String,
    repo: Option<String>,
    branch: Option<String>,
    model: Option<String>,
    max_budget_usd: Option<f64>,
}

async fn create_trigger(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<CreateTriggerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let (row, token) = crate::triggers::create(&state.pool, crate::triggers::NewTrigger {
        org_id: ctx.org_id,
        user_id: ctx.user_id,
        name: req.name,
        prompt: req.prompt,
        repo: req.repo,
        branch: req.branch,
        model: req.model,
        max_budget_usd: req.max_budget_usd,
    })
    .await
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "trigger.create", &row.id.to_string(), serde_json::json!({}))
        .await
        .ok();
    // The token is shown exactly once, like every other credential here.
    Ok(Json(serde_json::json!({
        "trigger": row,
        "url": format!("/v1/hooks/{token}"),
        "token": token,
        "note": "store this now — it is not recoverable",
    })))
}

async fn list_triggers(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<Vec<crate::triggers::TriggerRow>>> {
    Ok(Json(crate::triggers::list(&state.pool, ctx.org_id).await?))
}

async fn delete_trigger(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !crate::triggers::remove(&state.pool, id, ctx.org_id).await? {
        return Err(AppError(StatusCode::NOT_FOUND, "trigger not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Inbound webhook: start a session from a trigger's template.
///
/// Unauthenticated in the usual sense — the token in the path is the
/// credential, which is what makes the URL pasteable into GitHub, Slack or
/// any other system that only knows how to POST somewhere.
async fn fire_hook(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(trigger) = crate::triggers::by_token(&state.pool, &token).await? else {
        // Same response for "no such token" and "disabled": a prober learns
        // nothing about which tokens exist.
        return Err(AppError(StatusCode::NOT_FOUND, "unknown hook".into()));
    };
    let payload = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);
    let session_id = crate::triggers::fire(&state, &trigger, &payload)
        .await
        .map_err(|e| conflict(e.to_string()))?;
    if let Err(e) = dispatch_pending(&state).await {
        tracing::warn!(error = %e, "dispatch after hook failed");
    }
    Ok(Json(serde_json::json!({"session_id": session_id})))
}

// ----------------------------------------------------------------- fleet

/// What is actually running, from both points of view.
///
/// The database says which sessions it believes are live; each worker
/// reports the sandboxes msb actually has. They agree almost always, and
/// the cases where they don't are the ones worth an operator's attention:
///
/// * `vanished` — a session the platform thinks is running whose VM is gone.
///   Its events stopped arriving and nothing noticed.
/// * `orphaned` — a sandbox with no live session behind it, holding a
///   couple of gigabytes of a worker's memory for nothing.
///
/// A worker that has not reported an inventory yet (older build, or no
/// heartbeat since connecting) is reported as `inventory: null` and
/// excluded from drift, so "we don't know" never renders as "everything
/// is broken".
async fn fleet(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<serde_json::Value>> {
    type Row = (Uuid, String, String, i32, i32, Option<chrono::DateTime<chrono::Utc>>, Vec<String>);
    let workers: Vec<Row> = sqlx::query_as(
        "SELECT id, name, status, capacity_slots, used_slots, last_heartbeat_at, engines \
         FROM workers WHERE status <> 'offline' ORDER BY name",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Non-admins see only their own live sessions; the worker rows carry no
    // tenant data, so the shape is the same either way.
    let user_filter = if ctx.admin { None } else { ctx.user_id };
    let live = db::live_sessions(&state.pool, if ctx.admin { None } else { Some(ctx.org_id) }, user_filter)
        .await?;

    let machines = db::machines::live(&state.pool, if ctx.admin { None } else { Some(ctx.org_id) })
        .await?;
    let known_machines: std::collections::HashSet<&str> =
        machines.iter().map(|m| m.name.as_str()).collect();

    let connected: std::collections::HashMap<Uuid, Option<Vec<String>>> = state
        .workers
        .all()
        .into_iter()
        .map(|w| (w.worker_id, w.sandboxes))
        .collect();

    let mut out = Vec::new();
    let mut vanished = Vec::new();
    let mut orphaned = Vec::new();

    for (id, name, status, capacity, used, heartbeat, engines) in workers {
        let inventory = connected.get(&id).cloned().flatten();
        let sessions: Vec<_> = live.iter().filter(|s| s.worker_id == Some(id)).collect();
        let on_worker: Vec<_> = machines.iter().filter(|m| m.worker_id == Some(id)).collect();

        if let Some(inv) = &inventory {
            for m in &on_worker {
                // Only a booted machine has a VM to miss.
                if matches!(m.state.as_str(), "running" | "stopping") && !inv.contains(&m.name) {
                    vanished.push(serde_json::json!({
                        "machine_id": m.id, "sandbox": m.name, "state": m.state, "worker": name,
                    }));
                }
            }
            for sandbox in inv {
                if sandbox.starts_with("mch-") && !known_machines.contains(sandbox.as_str()) {
                    orphaned.push(serde_json::json!({"sandbox": sandbox, "worker": name}));
                }
            }
            for s in &sessions {
                // `scheduled` means "placed, not yet booted" — there is no
                // sandbox to miss, so it is not drift.
                if s.state == "scheduled" {
                    continue;
                }
                if !inv.contains(&s.sandbox_name) {
                    vanished.push(serde_json::json!({
                        "session_id": s.id, "sandbox": s.sandbox_name,
                        "state": s.state, "worker": name,
                    }));
                }
            }
            let known: std::collections::HashSet<&str> =
                live.iter().map(|s| s.sandbox_name.as_str()).collect();
            for sandbox in inv {
                // Only `ses-` names are ours; anything else on the host
                // belongs to another tenant of that machine.
                if sandbox.starts_with("ses-") && !known.contains(sandbox.as_str()) {
                    orphaned.push(serde_json::json!({"sandbox": sandbox, "worker": name}));
                }
            }
        }

        out.push(serde_json::json!({
            "id": id,
            "name": name,
            "status": status,
            "capacity_slots": capacity,
            "used_slots": used,
            "last_heartbeat_at": heartbeat,
            "connected": connected.contains_key(&id),
            "engines": engines,
            "inventory": inventory,
            "sessions": sessions.iter().map(|s| serde_json::json!({
                "id": s.id,
                "sandbox": s.sandbox_name,
                "title": s.title,
                "state": s.state,
                "engine": s.engine,
                "started_at": s.started_at,
                "cost_usd": s.cost_usd,
                "user_id": s.user_id,
                "repo": s.repo,
                "waiting": s.pending_question.is_some(),
            })).collect::<Vec<_>>(),
            "machines": on_worker.iter().map(|m| serde_json::json!({
                "id": m.id,
                "sandbox": m.name,
                "state": m.state,
                "engine": m.engine,
                "memory_mib": m.memory_mib,
                "external_id": m.external_id,
            })).collect::<Vec<_>>(),
        }));
    }

    Ok(Json(serde_json::json!({
        "workers": out,
        "drift": {"vanished": vanished, "orphaned": orphaned},
    })))
}

// --------------------------------------------------------------- workers

async fn list_workers(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    if !ctx.admin {
        return Err(forbidden());
    }
    /// (id, name, status, capacity_slots, used_slots, last_heartbeat_at, engines, features)
    type WorkerRow = (
        Uuid,
        String,
        String,
        i32,
        i32,
        Option<chrono::DateTime<chrono::Utc>>,
        Vec<String>,
        Vec<String>,
    );
    let rows: Vec<WorkerRow> =
        sqlx::query_as(
            "SELECT id, name, status, capacity_slots, used_slots, last_heartbeat_at, \
                    engines, features \
             FROM workers ORDER BY name",
        )
        .fetch_all(&state.pool)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name, status, cap, used, hb, engines, features)| {
                serde_json::json!({
                    "id": id, "name": name, "status": status,
                    "capacity_slots": cap, "used_slots": used,
                    "last_heartbeat_at": hb, "connected": state.workers.get(id).is_some(),
                    "engines": engines, "features": features,
                })
            })
            .collect(),
    ))
}

async fn set_drain(state: &AppState, ctx: &AuthCtx, id: Uuid, draining: bool) -> ApiResult<StatusCode> {
    if !ctx.admin {
        return Err(forbidden());
    }
    state.workers.set_draining(id, draining);
    let status = if draining { "draining" } else { "online" };
    sqlx::query("UPDATE workers SET status = $2 WHERE id = $1 AND status != 'offline'")
        .bind(id)
        .bind(status)
        .execute(&state.pool)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    db::audit(&state.pool, Some(ctx.org_id), ctx.user_id, "worker.drain", &id.to_string(), serde_json::json!({"draining": draining}))
        .await
        .ok();
    Ok(StatusCode::ACCEPTED)
}

#[derive(serde::Deserialize)]
struct MemorySettingsReq {
    enabled: bool,
}

#[derive(serde::Serialize)]
struct MemorySettingsResp {
    enabled: bool,
    /// Today's call volume. Cloudflare Agent Memory is an unpriced private
    /// beta, so this is the only warning an operator gets before an invoice.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_today: Option<serde_json::Value>,
    /// False when the deployment runs no memory service at all, in which
    /// case `enabled` is inert and a UI should say so rather than offer a
    /// toggle that does nothing.
    available: bool,
}

/// Read the org's memory setting.
async fn get_memory_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
) -> ApiResult<Json<MemorySettingsResp>> {
    Ok(Json(MemorySettingsResp {
        enabled: db::memory_enabled(&state.pool, ctx.org_id).await,
        available: state.memory.is_some(),
        usage_today: db::memory_usage_today(&state.pool, ctx.org_id).await,
    }))
}

/// Turn memory on or off for the caller's org.
///
/// Admin-only, and audited, because enabling it starts sending distilled
/// transcript text to another service — and behind that, to Cloudflare. That
/// is a data-egress decision, not a preference.
async fn set_memory_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Json(req): Json<MemorySettingsReq>,
) -> ApiResult<Json<MemorySettingsResp>> {
    if !ctx.admin {
        return Err(forbidden());
    }
    sqlx::query("UPDATE orgs SET memory_enabled = $2 WHERE id = $1")
        .bind(ctx.org_id)
        .bind(req.enabled)
        .execute(&state.pool)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    db::audit(
        &state.pool,
        Some(ctx.org_id),
        ctx.user_id,
        "memory.settings",
        &ctx.org_id.to_string(),
        serde_json::json!({"enabled": req.enabled}),
    )
    .await
    .ok();
    Ok(Json(MemorySettingsResp {
        enabled: req.enabled,
        available: state.memory.is_some(),
        usage_today: db::memory_usage_today(&state.pool, ctx.org_id).await,
    }))
}

#[derive(serde::Serialize)]
struct MemoryProfileResp {
    profile_id: Option<String>,
    /// Rendered preamble, so an operator can read exactly what sessions on
    /// this repo are being told before deciding whether it is any good.
    preamble: Option<String>,
}

/// What a session on `?repo=` would currently be told.
///
/// Proxied through controld rather than exposing the memory service directly:
/// the service key never leaves the control plane, and the org comes from the
/// verified caller rather than from a query parameter.
async fn get_memory_profile(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<MemoryProfileResp>> {
    let Some(client) = &state.memory else {
        return Ok(Json(MemoryProfileResp { profile_id: None, preamble: None }));
    };
    let repo = q.get("repo").map(|s| s.as_str()).filter(|s| !s.is_empty());
    let profile = client
        .resolve(ctx.org_id, repo)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, format!("memory service: {e:#}")))?;
    let preamble = client
        // The caller's own personal layer: this endpoint answers "what would
        // my next session be told", and for them that includes their own
        // preferences and nobody else's.
        .preamble(
            &profile.id,
            state.cfg.memory_preamble_bytes,
            ctx.user_id.map(|u| u.to_string()).as_deref(),
        )
        .await
        .ok()
        .flatten()
        .map(|p| p.preamble);
    Ok(Json(MemoryProfileResp { profile_id: Some(profile.id), preamble }))
}

async fn drain_worker(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    set_drain(&state, &ctx, id, true).await
}

async fn undrain_worker(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    set_drain(&state, &ctx, id, false).await
}

// ------------------------------------------------------------- dispatcher

/// Resolve which credential a session runs on, in preference order:
/// the caller's own (captured at create/resume), then the org's stored
/// fallback, then the operator's global key.
///
/// This is the fix for "usage is metered per org but every org spends the
/// operator's single credential".
/// puku platform credentials authenticate as bearers, whatever the stored
/// `kind` says. Anthropic keys (`sk-ant-...`) are the x-api-key case.
fn is_puku_platform_key(value: &str) -> bool {
    let v = value.trim();
    v.starts_with("pk_live_") || v.starts_with("pk_test_") || v.starts_with("pkc_")
}

pub(crate) async fn resolve_credential(state: &AppState, s: &SessionRow) -> Option<(String, String)> {
    let secrets = state.secrets.as_ref()?;
    // The caller's own credential, captured when the session was created,
    // always wins: it is the identity that asked for this work.
    match db::session_credential(&state.pool, s.id).await {
        Ok(Some((kind, enc))) => match secrets.decrypt(&enc) {
            Ok(value) => return Some((kind, value)),
            Err(e) => tracing::warn!(session = %s.id, error = %e, "session credential unreadable"),
        },
        Ok(None) => {}
        Err(e) => tracing::warn!(session = %s.id, error = %e, "reading the session credential failed"),
    }
    org_credential_for(state, s.org_id, s.id).await
}

/// A stored bearer this dispatch may use as-is, if there is one.
///
/// A cached bearer saves an issuer round-trip when a burst of scheduled runs
/// fires at once, so it is preferred -- but only when it can go stale safely.
///
/// A *minted* bearer carries the expiry it was minted with, and
/// `org_credentials_all` drops it the moment that passes. A *hand-stored* one
/// carries no expiry at all, so nothing ever drops it. Preferring that
/// unconditionally meant a stored bearer outranked a refresh token for ever:
/// once the issuer revoked it, every scheduled run in the org 401'd inside a
/// guest with a perfectly good refresh token sitting unused beside it.
///
/// So a bearer with no expiry is only used when the org has no refresh token
/// to mint from -- where it is still the best thing available.
fn usable_bearer(rows: &[db::OrgCredentialRow]) -> Option<usize> {
    let has_refresh = rows.iter().any(|(k, _, _, _)| k == "refresh");
    rows.iter()
        .position(|(k, _, exp, _)| k == "bearer" && (exp.is_some() || !has_refresh))
}

/// The org's credential, minting one from a refresh token when that is what
/// is stored.
///
/// Order matters. A cached bearer is used while it is fresh so a burst of
/// scheduled runs does not hammer the issuer; otherwise a refresh token is
/// spent to mint a new one; an api_key is the last resort, and only helps on
/// deployments whose gateway accepts one.
async fn org_credential_for(
    state: &AppState,
    org_id: Uuid,
    session_id: Uuid,
) -> Option<(String, String)> {
    let secrets = state.secrets.as_ref()?;
    let rows = db::org_credentials_all(&state.pool, org_id)
        .await
        .map_err(|e| tracing::warn!(org = %org_id, error = %e, "reading org credentials failed"))
        .ok()?;
    let decrypt = |enc: &[u8]| secrets.decrypt(enc).ok();

    if let Some(i) = usable_bearer(&rows) {
        if let Some(v) = decrypt(&rows[i].1) {
            return Some(("bearer".to_string(), v));
        }
    }

    if let Some((_, enc, _, user_id)) = rows.iter().find(|(k, _, _, _)| k == "refresh") {
        if let Some(refresh_token) = decrypt(enc) {
            match crate::oauth::refresh(&state.cfg.auth_issuer, &refresh_token).await {
                Ok(minted) => {
                    // Cache the bearer so the next dispatch does not mint
                    // again, and store the rotated refresh token: issuers
                    // that rotate invalidate the one just spent, and losing
                    // it would break every future run.
                    if let Some(exp) = crate::oauth::expiry_from(minted.expires_in) {
                        if let Ok(enc) = secrets.encrypt(&minted.access_token) {
                            let _ = db::put_org_credential(
                                &state.pool, org_id, *user_id, "bearer", enc, Some(exp),
                            )
                            .await;
                        }
                    }
                    if let Some(rotated) = &minted.refresh_token {
                        if rotated != &refresh_token {
                            if let Ok(enc) = secrets.encrypt(rotated) {
                                let _ = db::put_org_credential(
                                    &state.pool, org_id, *user_id, "refresh", enc, None,
                                )
                                .await;
                            }
                        }
                    }
                    return Some(("bearer".to_string(), minted.access_token));
                }
                Err(e) => {
                    // Say so loudly: a spent refresh token breaks every
                    // scheduled run in this org, and the symptom downstream
                    // is an unexplained 401 inside a guest.
                    tracing::error!(
                        org = %org_id, session = %session_id, error = format!("{e:#}"),
                        "minting a bearer from the stored refresh token failed; \
                         its owner needs to store a new one"
                    );
                }
            }
        }
    }

    let (_, enc, _, _) = rows.iter().find(|(k, _, _, _)| k == "api_key")?;
    decrypt(enc).map(|v| ("api_key".to_string(), v))
}

/// A session's tool list, or the deployment's default when it named none.
///
/// Deliberately not a merge and not a ceiling. Naming any tool is a caller
/// saying "I have thought about this", and silently unioning the operator's
/// list into theirs would produce a policy neither party wrote. The empty
/// list is the only signal we have that they did not think about it.
fn default_tools(requested: &[String], fallback: &[String]) -> Vec<String> {
    if requested.is_empty() {
        fallback.to_vec()
    } else {
        requested.to_vec()
    }
}

fn build_spec(state: &AppState, s: &SessionRow, git_token: Option<String>) -> SessionSpec {
    // A session with a recorded puku session id has run before: this
    // assignment is a resume on existing volumes.
    let resume = s.puku_session_id.is_some();
    SessionSpec {
        session_id: s.id,
        sandbox_name: s.sandbox_name.clone(),
        engine: s.session_engine(),
        image: state.cfg.agent_image.clone(),
        cpus: 2,
        memory_mib: 2048,
        idle_timeout_s: s.idle_timeout_s as u32,
        max_duration_s: s.max_duration_s as u32,
        prompt: s.prompt.clone(),
        repo: s.repo.clone(),
        branch: s.branch.clone(),
        // A GitHub App installation token is scoped to the repo being
        // cloned, so it is always safe. The static PAT is not: it clones
        // whatever it can read, for whichever caller names a repo.
        git_token: git_token.or_else(|| state.cfg.operator_credential(&state.cfg.git_token)),
        model: s.model.clone(),
        max_budget_usd: s.max_budget_usd,
        // The deployment's defaults fill in only for a session that named
        // no policy of its own. Applied here rather than in the handlers so
        // the scheduler and the webhook-trigger path get them too -- triggers
        // build their sessions with empty lists and have no request to
        // inherit from, so before this they could not express a tool policy
        // at all.
        allowed_tools: default_tools(&s.allowed_tools, &state.cfg.default_allowed_tools),
        disallowed_tools: default_tools(&s.disallowed_tools, &state.cfg.default_disallowed_tools),
        // Clamped again on the way out: rows written before the ceiling
        // existed (or by a future admin path) never escape it, and an
        // unparseable value degrades to the ceiling rather than to god-mode.
        permission_mode: Some(
            s.permission_mode
                .as_deref()
                .and_then(PermissionMode::parse)
                .map(|m| m.clamp_to(state.cfg.permission_ceiling))
                .unwrap_or(state.cfg.permission_ceiling),
        ),
        max_turns: Some(
            s.max_turns
                .filter(|t| *t > 0)
                .map(|t| t as u32)
                .unwrap_or(state.cfg.default_max_turns),
        ),
        puku_api_key: state.cfg.operator_credential(&state.cfg.puku_api_key),
        puku_oauth_token: state.cfg.operator_credential(&state.cfg.puku_oauth_token),
        // Filled in by the dispatcher, which can await the decrypt, the
        // connector lookup and the skill resolve.
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        output_schema: s.output_schema.clone(),
        // Replayed verbatim on resume; the dispatcher fills it in on the
        // first dispatch only.
        memory_preamble: s.memory_preamble.clone(),
        puku_auth_token: None,
        puku_api_base: Some(state.cfg.api_url.clone()),
        // Read fresh each dispatch: puku-cli on this machine refreshes the
        // file, and new sessions should get the newest tokens.
        puku_session_json: state
            .cfg
            .allow_operator_credentials
            .then_some(state.cfg.puku_session_file.as_ref())
            .flatten()
            .and_then(|p| {
                std::fs::read_to_string(p)
                    .map_err(|e| tracing::warn!(path = %p.display(), error = %e, "session file unreadable"))
                    .ok()
            }),
        resume,
        events_cursor: s.last_guest_line,
        puku_session_id: s.puku_session_id.clone(),
        // Filled in by the dispatcher, which can await the presign.
        import: None,
    }
}

/// How long a session waits for the worker holding its volumes before it is
/// failed. Long enough to ride out a workerd restart or a box reboot; short
/// enough that a host that is not coming back does not strand a session in
/// `scheduled` indefinitely with nothing saying why.
const VOLUME_HOST_GRACE: chrono::Duration = chrono::Duration::minutes(15);

/// Fail a session whose volumes are on a worker that has been gone too long.
///
/// A connected worker that is merely full, or not running the engine right
/// now, is waited on: that resolves on its own. Only absence past the grace
/// period is final, because the workspace exists on that disk and nowhere
/// else.
async fn fail_if_volume_host_gone(
    state: &AppState,
    session: &SessionRow,
    pinned: Uuid,
) -> anyhow::Result<()> {
    if state.workers.get(pinned).is_some() {
        return Ok(());
    }
    let presence = db::worker_presence(&state.pool, pinned).await?;
    let absent_for = match &presence {
        Some((_, _, Some(hb))) => chrono::Utc::now() - *hb,
        _ => VOLUME_HOST_GRACE + chrono::Duration::seconds(1),
    };
    if absent_for <= VOLUME_HOST_GRACE {
        return Ok(());
    }
    let name = presence.map(|p| p.0).unwrap_or_else(|| "an unknown worker".into());
    let reason = format!(
        "the worker holding this session's volumes ({name}) has been offline for more than \
         {} minutes; the workspace exists only there, so the session cannot resume elsewhere",
        VOLUME_HOST_GRACE.num_minutes()
    );
    tracing::warn!(session = %session.id, worker = %name, "{reason}");
    if let Ok((_, Some(ev))) =
        db::transition(&state.pool, session.id, SessionState::Failed, Some(&reason)).await
    {
        state.publish_events(&[ev]).await;
    }
    Ok(())
}

/// Place undispatched sessions on online workers. Called after session
/// creation, on worker registration, and from the periodic retry loop.
pub async fn dispatch_pending(state: &AppState) -> anyhow::Result<()> {
    let pending = db::dispatchable_sessions(&state.pool).await?;
    for session in pending {
        let engine = session.session_engine();
        if engine == Engine::Unsupported {
            // Only reachable through a row the CHECK constraint should have
            // refused; say what is wrong rather than queueing it for ever.
            let reason = format!("session names an engine this control plane does not know: {:?}", session.engine);
            if let Ok((_, Some(ev))) =
                db::transition(&state.pool, session.id, SessionState::Failed, Some(&reason)).await
            {
                state.publish_events(&[ev]).await;
            }
            continue;
        }
        let placement = Placement { engine, pinned: session.volume_worker_id, ..Placement::default() };
        // `continue`, not `return`: with engines and pinning in play, one
        // session nobody can take says nothing about the next. Returning here
        // let a queued Cloud Hypervisor session block every libkrun session
        // behind it until a Cloud Hypervisor worker appeared.
        let Some(worker) = state.workers.pick(&placement) else {
            if let Some(pinned) = placement.pinned {
                fail_if_volume_host_gone(state, &session, pinned).await?;
            }
            continue;
        };
        // GitHub App tokens beat the static PAT when configured.
        let git_token = match (&*state.github, &session.repo) {
            (Some(app), Some(repo)) => match app.installation_token(repo).await {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!(session = %session.id, error = format!("{e:#}"), "github token mint failed; falling back to static token");
                    None
                }
            },
            _ => None,
        };
        // Lost the race to a concurrent dispatch_pending. That call owns the
        // session and will send the assignment; sending a second would boot a
        // second VM for one session (the worker dedups, but only after both
        // slots are gone).
        if !db::assign_worker(&state.pool, session.id, worker.worker_id).await? {
            continue;
        }
        match db::transition(&state.pool, session.id, SessionState::Scheduled, None).await {
            Ok((_, Some(ev))) => state.publish_events(&[ev]).await,
            Ok((_, None)) => {}
            Err(e) => {
                tracing::warn!(session = %session.id, error = %e, "schedule transition failed");
                continue;
            }
        }
        let mut spec = build_spec(state, &session, git_token);
        match resolve_credential(state, &session).await {
            // `api_key` describes where the value came from, not how it
            // authenticates. An Anthropic key rides the x-api-key header;
            // a puku platform key is a bearer for the puku gateway. Routing
            // a pk_live_ key to PUKU_AI_API_KEY sends it as x-api-key, the
            // gateway never sees a bearer, and it answers
            //   401 {"code":"missing_token","message":"Authentication required"}
            // -- which reads as a revoked key rather than a misrouted one.
            Some((kind, value)) if kind == "api_key" && !is_puku_platform_key(&value) => {
                spec.puku_api_key = Some(value)
            }
            Some((_, value)) => spec.puku_auth_token = Some(value),
            None => {} // operator global, already on the spec
        }
        // Booting a VM that will 401 on its first model call wastes a slot
        // and reports as a confusing agent failure. Refuse up front and say
        // which knob is missing. This is the common shape of a broken cron
        // job on a multi-user deployment: the owner never stored a
        // credential and the operator set no global fallback.
        if spec.puku_api_key.is_none()
            && spec.puku_auth_token.is_none()
            && spec.puku_oauth_token.is_none()
            && spec.puku_session_json.is_none()
        {
            let reason = "no model credential for this session: its owner has not stored one. \
                 Store a refresh token with POST /v1/credentials {\"kind\":\"refresh\"} so \
                 unattended runs keep working, or a bearer for a one-off. This deployment \
                 does not fall back to an operator-wide credential (see \
                 --allow-operator-credentials)";
            tracing::warn!(session = %session.id, "{reason}");
            if let Ok((_, Some(ev))) =
                db::transition(&state.pool, session.id, SessionState::Failed, Some(reason)).await
            {
                state.publish_events(&[ev]).await;
            }
            sqlx::query("UPDATE sessions SET worker_id = NULL WHERE id = $1")
                .bind(session.id)
                .execute(&state.pool)
                .await?;
            continue;
        }
        // Recalled context, resolved at dispatch like skills and connectors,
        // and additive in exactly the same way: it can only ever make the
        // session better informed, never fail it.
        crate::memory::attach_preamble(state, &session, &mut spec).await;

        // A teleported session needs its transcript in place before the
        // runner starts, or `--resume` finds nothing and the agent looks
        // like it forgot the conversation.
        if session.puku_session_id.is_some() {
            spec.import = resolve_import(state, session.id).await;
        }
        // Skills, resolved fresh at dispatch so a resumed session picks up
        // the current pack version rather than a stale presigned URL.
        if let Some(registry) = &state.skills {
            match registry
                .resolve(spec.puku_auth_token.as_deref(), session.org_id, &session.packs)
                .await
            {
                Ok(packs) => {
                    if !packs.is_empty() {
                        tracing::info!(
                            session = %session.id,
                            packs = ?packs.iter().map(|p| format!("{}@{}", p.name, p.version)).collect::<Vec<_>>(),
                            "resolved skill packs"
                        );
                    }
                    spec.skills = packs;
                }
                Err(e) => {
                    // A session explicitly told to use a pack must not run
                    // without it: the agent would look incompetent rather
                    // than mis-configured. With no packs requested this is
                    // just the registry being down, so carry on.
                    if !session.packs.is_empty() {
                        let reason = format!("could not resolve skill packs: {e:#}");
                        tracing::warn!(session = %session.id, "{reason}");
                        if let Ok((_, Some(ev))) = db::transition(
                            &state.pool, session.id, SessionState::Failed, Some(&reason)).await
                        {
                            state.publish_events(&[ev]).await;
                        }
                        sqlx::query("UPDATE sessions SET worker_id = NULL WHERE id = $1")
                            .bind(session.id)
                            .execute(&state.pool)
                            .await?;
                        continue;
                    }
                    tracing::warn!(session = %session.id, error = %e, "skill resolve failed; continuing without skills");
                }
            }
        }

        // Connectors are brokered per user, so they need the caller's own
        // bearer — an operator key would list the operator's connections.
        if session.connectors {
            if let (Some(client), Some(bearer)) = (&state.connectors, &spec.puku_auth_token) {
                spec.mcp_servers = client.servers_for(bearer).await;
                if !spec.mcp_servers.is_empty() {
                    tracing::info!(
                        session = %session.id,
                        count = spec.mcp_servers.len(),
                        "attached brokered connectors"
                    );
                }
            }
        }
        if !worker.send(Down::AssignSession { spec }) {
            // Worker vanished between pick and send; undo the claim so the
            // retry loop can re-place the session.
            sqlx::query("UPDATE sessions SET worker_id = NULL WHERE id = $1")
                .bind(session.id)
                .execute(&state.pool)
                .await?;
        } else {
            tracing::info!(session = %session.id, worker = %worker.name, "session dispatched");
        }
    }
    // Machines share the workers, the slots and every trigger that makes
    // dispatch worth retrying (a create, a registration, the periodic tick).
    machines::dispatch_machines(state).await
}

// ------------------------------------------------------------ websockets

async fn worker_ws(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| crate::workerlink::handle_worker_socket(state, socket))
}

async fn attach(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| attach_socket(state, ctx, id, socket))
}

async fn send_msg(
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMsg,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).unwrap();
    sink.send(Message::Text(text.into())).await
}

async fn attach_socket(state: AppState, ctx: AuthCtx, session_id: Uuid, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // First message must be Hello.
    let after_seq = match stream.next().await {
        Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMsg>(&t) {
            Ok(ClientMsg::Hello { after_seq }) => after_seq,
            _ => {
                let _ = send_msg(&mut sink, &ServerMsg::Error { message: "expected hello".into() }).await;
                return;
            }
        },
        _ => return,
    };

    // `owns`, the same check every REST route on a session makes. This used
    // to compare org only, so any user in an org could attach to -- and
    // answer, type into, and interrupt -- another user's session.
    let session = match db::get_session(&state.pool, session_id).await {
        Ok(Some(s)) if owns(&ctx, s.org_id, s.user_id) => s,
        _ => {
            let _ = send_msg(&mut sink, &ServerMsg::Error { message: "session not found".into() }).await;
            return;
        }
    };

    // Subscribe BEFORE replaying so nothing published during replay is lost;
    // `max_sent` dedups the overlap.
    let mut rx = state.hub.subscribe(session_id);
    let mut max_sent: i64 = after_seq.max(0);

    if after_seq >= 0 {
        let mut cursor = after_seq;
        loop {
            let batch = match db::fetch_events(&state.pool, session_id, cursor, 200).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = send_msg(&mut sink, &ServerMsg::Error { message: e.to_string() }).await;
                    return;
                }
            };
            if batch.is_empty() {
                break;
            }
            cursor = batch.last().unwrap().seq;
            max_sent = cursor;
            if send_msg(&mut sink, &ServerMsg::Events { events: batch }).await.is_err() {
                return;
            }
        }
    } else {
        max_sent = session.last_seq;
    }

    let _ = send_msg(&mut sink, &ServerMsg::Live).await;
    let _ = send_msg(&mut sink, &ServerMsg::State {
        state: session.session_state(),
        error: session.error.clone(),
    })
    .await;

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) if ev.seq > max_sent => {
                    max_sent = ev.seq;
                    if send_msg(&mut sink, &ServerMsg::Events { events: vec![ev] }).await.is_err() {
                        return;
                    }
                }
                Ok(_) => {} // already sent during replay
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Fall back to a DB catch-up after lag.
                    if let Ok(batch) = db::fetch_events(&state.pool, session_id, max_sent, 2000).await {
                        if let Some(last) = batch.last() {
                            max_sent = last.seq;
                            if send_msg(&mut sink, &ServerMsg::Events { events: batch }).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(_) => return,
            },
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    _ => continue,
                };
                let Ok(cmsg) = serde_json::from_str::<ClientMsg>(&text) else { continue };
                let Ok(Some(session)) = db::get_session(&state.pool, session_id).await else { return };
                let result = match cmsg {
                    ClientMsg::Hello { .. } => Ok(()),
                    ClientMsg::Answer { question_id, answer, answers, decision, message } =>
                        deliver_answer(&state, &session, &AnswerReq {
                            question_id, answer, answers, decision, message,
                        })
                        .await,
                    ClientMsg::Input { text } =>
                        deliver_user_text(&state, &session, &text).await,
                    ClientMsg::Interrupt =>
                        send_to_worker(&state, session.worker_id, Down::Interrupt { session_id }),
                };
                if let Err(e) = result {
                    let _ = send_msg(&mut sink, &ServerMsg::Error { message: e.to_string() }).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod answer_tests {
    use super::{build_control_response, AnswerReq};

    fn req(answer: Option<&str>) -> AnswerReq {
        AnswerReq {
            question_id: "req-1".into(),
            answer: answer.map(|s| s.to_string()),
            answers: None,
            decision: None,
            message: None,
        }
    }

    /// Shaped exactly like what workerd stores after seeing a real
    /// `can_use_tool` frame from puku-cli 1.8.43.
    fn pending() -> serde_json::Value {
        serde_json::json!({
            "kind": "can_use_tool",
            "request_id": "req-1",
            "tool_name": "AskUserQuestion",
            "tool_use_id": "call_abc",
            "input": {"questions": [{
                "question": "Do you prefer tabs or spaces?",
                "header": "Indentation",
                "options": [{"label": "Tabs"}, {"label": "Spaces"}],
                "multiSelect": false,
            }]},
        })
    }

    /// The frame puku-cli is blocked on. Verified end-to-end against the
    /// real CLI: `updatedInput.answers` keyed by the question's `header`
    /// comes back to the model as `"Indentation"="Tabs"`.
    #[test]
    fn builds_the_frame_puku_cli_is_waiting_for() {
        let line = build_control_response(&pending(), &req(Some("Tabs"))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "req-1");
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        assert_eq!(
            v["response"]["response"]["updatedInput"]["answers"]["Indentation"],
            "Tabs"
        );
        // The original questions must survive alongside the answers.
        assert_eq!(
            v["response"]["response"]["updatedInput"]["questions"][0]["header"],
            "Indentation"
        );
    }

    /// puku-cli keys answers by `header || Q{n}`; match that fallback or the
    /// answer silently lands under a key the CLI never reads.
    #[test]
    fn falls_back_to_positional_keys_when_a_question_has_no_header() {
        let mut p = pending();
        p["input"]["questions"][0]
            .as_object_mut()
            .unwrap()
            .remove("header");
        let line = build_control_response(&p, &req(Some("Tabs"))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["response"]["response"]["updatedInput"]["answers"]["Q1"], "Tabs");
    }

    #[test]
    fn per_question_answers_are_routed_by_header() {
        let mut p = pending();
        p["input"]["questions"] = serde_json::json!([
            {"question": "tabs or spaces?", "header": "Indentation"},
            {"question": "which linter?", "header": "Linter"},
        ]);
        let mut r = req(None);
        r.answers = Some(
            [("Indentation".to_string(), "Tabs".to_string()),
             ("Linter".to_string(), "clippy".to_string())]
                .into_iter()
                .collect(),
        );
        let line = build_control_response(&p, &r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let answers = &v["response"]["response"]["updatedInput"]["answers"];
        assert_eq!(answers["Indentation"], "Tabs");
        assert_eq!(answers["Linter"], "clippy");
    }

    /// Denying is a legitimate answer: the CLI turns it into an errored
    /// tool_result and the agent routes around it.
    #[test]
    fn deny_carries_a_message_and_no_answers() {
        let mut r = req(None);
        r.decision = Some("deny".into());
        r.message = Some("not now".into());
        let line = build_control_response(&pending(), &r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["response"]["response"]["behavior"], "deny");
        assert_eq!(v["response"]["response"]["message"], "not now");
    }

    /// A stale answer (the user answered the previous question) must not
    /// unblock the current one — the CLI would ignore the mismatched id and
    /// the session would look answered while still being stuck.
    #[test]
    fn refuses_an_answer_for_a_different_question() {
        let mut r = req(Some("Tabs"));
        r.question_id = "some-other-request".into();
        let err = build_control_response(&pending(), &r).unwrap_err().to_string();
        assert!(err.contains("no longer pending"), "{err}");
    }

    /// A bare permission ask ("may I run Bash?") has no questions to answer;
    /// allow alone satisfies it.
    #[test]
    fn permission_only_ask_needs_no_answer_text() {
        let p = serde_json::json!({
            "kind": "can_use_tool",
            "request_id": "req-1",
            "tool_name": "Bash",
            "input": {"command": "ls"},
        });
        let line = build_control_response(&p, &req(None)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        assert_eq!(v["response"]["response"]["updatedInput"]["command"], "ls");
    }

    /// But a question that expects an answer must not be silently allowed
    /// with an empty one: that is what produced `"answered your questions: ."`
    /// and a model free to invent the reply.
    #[test]
    fn refuses_to_allow_a_question_with_no_answer() {
        let err = build_control_response(&pending(), &req(None)).unwrap_err().to_string();
        assert!(err.contains("answer required"), "{err}");
    }

    /// A question raised by the SDK runner is answered with a plain line, not
    /// a control_response: that runner owns the CLI's control channel and
    /// cannot be handed an envelope for it.
    fn pending_platform() -> serde_json::Value {
        let mut p = pending();
        p["kind"] = serde_json::json!("platform.question");
        p
    }

    #[test]
    fn a_platform_question_is_answered_in_the_platform_dialect() {
        let req = AnswerReq {
            question_id: "req-1".into(),
            answer: None,
            answers: Some(std::collections::HashMap::from([("Indentation".to_string(), "Spaces".to_string())])),
            decision: None,
            message: None,
        };
        let line = build_control_response(&pending_platform(), &req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "platform.answer");
        assert_eq!(v["request_id"], "req-1");
        assert_eq!(v["decision"], "allow");
        assert_eq!(v["answers"]["Indentation"], "Spaces");
        // The runner rebuilds updatedInput itself, so the platform must not.
        assert!(v.get("response").is_none());
    }

    /// Regression: the platform branch used to skip this check, so an allow
    /// with nothing in it was accepted, cleared pending_question, and made
    /// the caller's real answer come back 409 on a question nobody could
    /// answer any more. Found by sending exactly that.
    #[test]
    fn a_platform_allow_with_no_answer_is_refused() {
        let req = AnswerReq {
            question_id: "req-1".into(),
            answer: None,
            answers: None,
            decision: None,
            message: None,
        };
        assert!(build_control_response(&pending_platform(), &req).is_err());
    }

    /// A bare permission ask ("may I run Bash?") carries no questions, so
    /// allow alone is the whole answer and must still work.
    #[test]
    fn a_platform_permission_only_ask_needs_no_answer_text() {
        let mut p = pending_platform();
        p["input"] = serde_json::json!({});
        let req = AnswerReq {
            question_id: "req-1".into(),
            answer: None,
            answers: None,
            decision: None,
            message: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&build_control_response(&p, &req).unwrap()).unwrap();
        assert_eq!(v["decision"], "allow");
    }

    #[test]
    fn a_platform_deny_carries_its_message() {
        let req = AnswerReq {
            question_id: "req-1".into(),
            answer: None,
            answers: None,
            decision: Some("deny".into()),
            message: Some("not in production".into()),
        };
        let v: serde_json::Value =
            serde_json::from_str(&build_control_response(&pending_platform(), &req).unwrap())
                .unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["message"], "not in production");
    }

    /// The staleness guard is dialect-independent; both runners rely on it to
    /// refuse an answer aimed at a question that has already been resolved.
    #[test]
    fn a_platform_question_still_refuses_a_stale_answer() {
        let req = AnswerReq {
            question_id: "some-other-id".into(),
            answer: Some("Spaces".into()),
            answers: None,
            decision: None,
            message: None,
        };
        assert!(build_control_response(&pending_platform(), &req).is_err());
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::owns;
    use crate::auth::AuthCtx;
    use uuid::Uuid;

    fn ctx(org: Uuid, user: Option<Uuid>, admin: bool) -> AuthCtx {
        AuthCtx { org_id: org, user_id: user, admin, bearer: None }
    }

    /// The isolation gap M6 closes: before per-user scoping, every member of
    /// an org could read, answer and cancel every other member's session.
    #[test]
    fn a_user_cannot_reach_another_users_session_in_the_same_org() {
        let org = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        assert!(!owns(&ctx(org, Some(bob), false), org, Some(alice)));
        assert!(owns(&ctx(org, Some(alice), false), org, Some(alice)));
    }

    #[test]
    fn other_orgs_are_never_visible() {
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let user = Uuid::new_v4();
        assert!(!owns(&ctx(mine, Some(user), false), theirs, Some(user)));
    }

    /// Admin keys run the fleet and must still see everything.
    #[test]
    fn admins_see_across_users_and_orgs() {
        let a = ctx(Uuid::new_v4(), Some(Uuid::new_v4()), true);
        assert!(owns(&a, Uuid::new_v4(), Some(Uuid::new_v4())));
    }

    /// Sessions created before user scoping have no owner. Scoping them to
    /// nobody would hide a running session from the person who started it.
    #[test]
    fn legacy_ownerless_sessions_stay_visible_to_their_org() {
        let org = Uuid::new_v4();
        assert!(owns(&ctx(org, Some(Uuid::new_v4()), false), org, None));
        assert!(!owns(&ctx(Uuid::new_v4(), Some(Uuid::new_v4()), false), org, None));
    }

    /// A bare org api key (CI, scripts) carries no user identity; it acts
    /// for the org, so it keeps org-wide access.
    #[test]
    fn org_scoped_api_keys_see_the_whole_org() {
        let org = Uuid::new_v4();
        assert!(owns(&ctx(org, None, false), org, Some(Uuid::new_v4())));
        assert!(!owns(&ctx(org, None, false), Uuid::new_v4(), Some(Uuid::new_v4())));
    }
}

#[cfg(test)]
mod health_query_tests {
    use super::HealthQuery;

    fn q(v: Option<&str>) -> HealthQuery {
        HealthQuery { deep: v.map(str::to_string) }
    }

    #[test]
    fn deep_defaults_off() {
        // The container HEALTHCHECK hits /health every 30s; it must not
        // bill an object-storage write each time.
        assert!(!q(None).deep());
    }

    #[test]
    fn deep_accepts_what_people_actually_type() {
        // serde's bool rejects "1", which is the whole reason this is a
        // String. A silently-ignored ?deep=1 would report the shallow
        // answer while the operator believed they had checked the
        // credentials -- worse than an error.
        for v in ["1", "true", "yes", ""] {
            assert!(q(Some(v)).deep(), "expected {v:?} to enable the probe");
        }
    }

    #[test]
    fn deep_rejects_negatives_and_junk() {
        for v in ["0", "false", "no", "maybe"] {
            assert!(!q(Some(v)).deep(), "expected {v:?} to leave the probe off");
        }
    }
}

#[cfg(test)]
mod credential_routing_tests {
    use super::is_puku_platform_key;

    #[test]
    fn puku_platform_keys_authenticate_as_bearers() {
        // Every one of these was stored as kind=api_key and silently sent as
        // x-api-key, which the puku gateway ignores.
        for k in ["pk_live_SIxPkTPrSt1TRrFKPz0", "pk_test_abc", "pkc_deadbeef", "  pk_live_x  "] {
            assert!(is_puku_platform_key(k), "{k} should route to the bearer header");
        }
    }

    #[test]
    fn anthropic_keys_still_ride_x_api_key() {
        for k in ["sk-ant-api03-xyz", "sk-ant-oat01-abc", ""] {
            assert!(!is_puku_platform_key(k), "{k} must stay on PUKU_AI_API_KEY");
        }
    }
}

#[cfg(test)]
mod credential_precedence_tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn row(kind: &str, expires_in_mins: Option<i64>) -> db::OrgCredentialRow {
        (
            kind.to_string(),
            vec![0u8],
            expires_in_mins.map(|m| Utc::now() + Duration::minutes(m)),
            None,
        )
    }

    /// The bug this function exists for.
    ///
    /// A bearer stored by hand has no expiry, so `org_credentials_all` never
    /// drops it. It used to win for ever, and the org's refresh token -- the
    /// one thing that would have kept unattended runs working -- was never
    /// reached.
    #[test]
    fn a_hand_stored_bearer_does_not_shadow_a_refresh_token() {
        let rows = vec![row("bearer", None), row("refresh", None)];
        assert_eq!(
            usable_bearer(&rows),
            None,
            "the refresh token must be minted from instead"
        );
    }

    /// With nothing to mint from, a stored bearer is still the best available
    /// credential -- refusing it would break single-credential orgs.
    #[test]
    fn a_hand_stored_bearer_is_used_when_there_is_no_refresh_token() {
        let rows = vec![row("api_key", None), row("bearer", None)];
        assert_eq!(usable_bearer(&rows), Some(1));
    }

    /// The round-trip saving this whole branch exists for: a bearer minted
    /// from a refresh token carries an expiry, so it is safe to reuse until
    /// the query drops it.
    #[test]
    fn a_minted_bearer_is_reused_while_it_is_live() {
        let rows = vec![row("bearer", Some(30)), row("refresh", None)];
        assert_eq!(
            usable_bearer(&rows),
            Some(0),
            "a live minted bearer should not force a fresh mint"
        );
    }

    #[test]
    fn no_bearer_at_all_is_not_a_bearer() {
        assert_eq!(usable_bearer(&[row("refresh", None)]), None);
        assert_eq!(usable_bearer(&[]), None);
    }
}

#[cfg(test)]
mod tool_policy_tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The case the knob exists for: a caller who expressed no policy gets
    /// the deployment's. This is what makes the WebSearch workaround
    /// applicable fleet-wide instead of once per request.
    #[test]
    fn an_empty_list_takes_the_deployment_default() {
        assert_eq!(default_tools(&[], &v(&["WebSearch", "WebFetch"])), v(&["WebSearch", "WebFetch"]));
    }

    /// Naming a tool is a caller saying they have thought about it. Merging
    /// the operator's list in would produce a policy neither party wrote.
    #[test]
    fn a_named_list_is_never_widened_or_merged() {
        let requested = v(&["Bash"]);
        assert_eq!(default_tools(&requested, &v(&["WebSearch"])), requested);
    }

    /// A caller may deliberately ask for *fewer* restrictions than the
    /// deployment default. That is allowed: this is a default, not a ceiling.
    #[test]
    fn a_caller_may_narrow_below_the_default() {
        let requested = v(&["WebFetch"]);
        assert_eq!(default_tools(&requested, &v(&["WebSearch", "WebFetch"])), requested);
    }

    /// The out-of-the-box deployment restricts nothing, so adding this knob
    /// cannot change behaviour for anyone who does not set it.
    #[test]
    fn an_unset_default_restricts_nothing() {
        assert!(default_tools(&[], &[]).is_empty());
    }
}
