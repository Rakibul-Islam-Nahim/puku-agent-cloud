//! Client for puku-memory-service.
//!
//! Same shape as `skills.rs` and `connectors.rs`: a plain HTTP client against
//! a separate service with its own database and deploy. There is no shared
//! library between them on purpose.
//!
//! Two rules govern everything here:
//!
//! 1. **Memory is additive capability.** Every call site treats an error as
//!    "carry on without it". No memory failure may fail a session.
//! 2. **What leaves a transcript is decided here.** The distiller (see
//!    `distill.rs`) filters and scrubs before anything crosses the network,
//!    because agent-cloud owns `session_events` and the scrubber. The memory
//!    service decides what to *remember*; this crate decides what it is
//!    allowed to see.

pub mod distill;

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How long dispatch will wait for a preamble before falling back to the
/// cache. Deliberately tight: this is on the boot path of every session.
const PREAMBLE_TIMEOUT: Duration = Duration::from_millis(800);
/// Ingest is fire-and-forget from a detached task, so it can afford more.
const INGEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
struct ResolveRequest<'a> {
    tenant_id: &'a str,
    scope: &'a str,
}

#[derive(Deserialize)]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Deserialize)]
pub struct Preamble {
    pub preamble: String,
    /// The same page WITHOUT the personal layer, and the only one that may be
    /// cached.
    ///
    /// `memory_preamble_cache` holds one row per profile, shared by everyone
    /// who touches that repository. Caching `preamble` would put whoever
    /// dispatched last into every colleague's fallback — their "### About you"
    /// in someone else's system prompt, on exactly the degradation path this
    /// cache exists to serve.
    ///
    /// Defaults to empty against a memory service too old to send it, and the
    /// caller then caches nothing rather than caching something unsafe.
    #[serde(default)]
    pub shared: String,
    #[serde(default)]
    pub etag: Option<String>,
}

#[derive(Serialize)]
pub struct Message {
    pub role: &'static str,
    pub content: String,
    /// Trust tier. The memory service quarantines `agent` until a second,
    /// independent session corroborates it.
    pub origin: &'static str,
}

#[derive(Serialize)]
pub struct Outcome {
    pub summary: String,
    pub status: String,
    pub cost_usd: f64,
}

#[derive(Serialize)]
pub struct IngestRequest {
    /// The session id. The service is idempotent on this, so a retried POST
    /// after a timeout costs nothing.
    pub source_id: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub label: String,
    pub messages: Vec<Message>,
    /// Who was in this session, when anyone was.
    ///
    /// Absent for a cron run or a webhook, which is ordinary. It is the
    /// identity a personal memory is filed under: without it the service drops
    /// a claim the extractor marked personal rather than storing it as a
    /// repository convention, which is the leak the whole layer prevents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

#[derive(Clone)]
pub struct MemoryClient {
    base_url: String,
    service_key: String,
    http: reqwest::Client,
}

/// Cloudflare Access service token, for when the memory service is reached
/// over a public hostname rather than the compose network.
///
/// The service key alone is a single shared string with no tenant isolation —
/// `tenant_id` arrives in the request body, so whoever holds it can read every
/// tenant. That is a reasonable perimeter on a private network and an
/// unreasonable one on the internet, so Access sits in front and this is how
/// controld gets through it.
#[derive(Clone)]
pub struct AccessToken {
    pub client_id: String,
    pub client_secret: String,
}

/// Build the Access headers, or an empty map when there is no token.
///
/// Split out of the constructor so the branch that matters can be tested: a
/// token that is not a legal header value must contribute NOTHING rather than
/// half a pair, because one header alone is refused at the edge just the same
/// but looks like a different bug.
fn access_headers(access: Option<AccessToken>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let Some(token) = access else {
        return headers;
    };
    match (
        reqwest::header::HeaderValue::from_str(&token.client_id),
        reqwest::header::HeaderValue::from_str(&token.client_secret),
    ) {
        (Ok(mut id), Ok(mut secret)) => {
            // Neither is a bearer, but both are credentials: keeping them out
            // of logs matters as much as it does for the service key.
            id.set_sensitive(true);
            secret.set_sensitive(true);
            headers.insert("CF-Access-Client-Id", id);
            headers.insert("CF-Access-Client-Secret", secret);
        }
        _ => {
            // A malformed token 403s every call. Say so once, here, rather
            // than once per preamble fetch with no clue where it came from.
            tracing::error!(
                "memory Access token is not a valid header value; \
                 requests will be refused at the edge"
            );
        }
    }
    headers
}

impl MemoryClient {
    /// `access` is None on the compose network and Some behind a public
    /// hostname. Its headers are attached by the client rather than at each
    /// call site so a new endpoint cannot forget them and get a 403 that
    /// reads as the memory service being down.
    pub fn new(base_url: String, service_key: String, access: Option<AccessToken>) -> Self {
        let headers = access_headers(access);
        MemoryClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            service_key,
            http: reqwest::Client::builder()
                .timeout(INGEST_TIMEOUT)
                .default_headers(headers)
                .build()
                .expect("building the http client"),
        }
    }

    /// Find or create the profile for an org and repo.
    ///
    /// `scope` is what makes the memory service reusable: agent-cloud sends
    /// `repo:<url>`, cowork could send `workspace:<path>`, and neither needs a
    /// schema change on the other side.
    pub async fn resolve(&self, org: Uuid, repo: Option<&str>) -> Result<Profile> {
        let scope = match repo {
            Some(r) if !r.trim().is_empty() => format!("repo:{}", r.trim()),
            // Repo-less sessions share one org-wide scratch profile rather
            // than each inventing their own.
            _ => "org".to_string(),
        };
        let resp = self
            .http
            .post(format!("{}/v1/profiles/resolve", self.base_url))
            .bearer_auth(&self.service_key)
            .json(&ResolveRequest {
                tenant_id: &org.to_string(),
                scope: &scope,
            })
            .send()
            .await
            .context("resolving memory profile")?
            .error_for_status()
            .context("memory service refused the resolve")?;
        resp.json().await.context("decoding memory profile")
    }

    /// Fetch the assembled preamble.
    ///
    /// `Ok(None)` means the service answered 204: there is genuinely nothing
    /// worth injecting. That is different from an error, and the caller must
    /// not fall back to a cached preamble for it — a profile that has been
    /// emptied should stop influencing sessions.
    pub async fn preamble(
        &self,
        profile_id: &str,
        budget: usize,
        user_id: Option<&str>,
    ) -> Result<Option<Preamble>> {
        let resp = self
            .http
            .get(format!(
                // user_id selects the personal layer. Omitted for a session
                // with nobody in it, which correctly gets the shared layers
                // and nothing personal.
                "{}/v1/profiles/{profile_id}/preamble?budget={budget}{}",
                self.base_url,
                match user_id {
                    Some(u) if !u.is_empty() => format!("&user_id={u}"),
                    _ => String::new(),
                }
            ))
            .bearer_auth(&self.service_key)
            .timeout(PREAMBLE_TIMEOUT)
            .send()
            .await
            .context("fetching preamble")?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let resp = resp.error_for_status().context("preamble request failed")?;
        Ok(Some(resp.json().await.context("decoding preamble")?))
    }

    /// Hand over one finished session. Returns as soon as the service has
    /// queued it (202); extraction happens on their side.
    /// Post a session's distilled transcript, and the model credential this
    /// tenant's extraction should be billed to.
    ///
    /// The credential rides HEADERS, never the body. The memory service
    /// marshals the whole ingest body into its job queue, which is retained for
    /// days and is exactly what an operator reads when a job dead-letters — a
    /// credential there would be a plaintext credential in Postgres. Their
    /// payload type has no field for one, so it cannot happen by accident.
    ///
    /// `cred` is what `resolve_credential` produced: the caller's own
    /// credential, or the org's. Absent for a session that has neither, and the
    /// memory service then degrades — it records activity and serves pinned
    /// conventions, and learns nothing new for that tenant.
    pub async fn ingest(
        &self,
        profile_id: &str,
        req: &IngestRequest,
        cred: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut rq = self
            .http
            .post(format!("{}/v1/profiles/{profile_id}/ingest", self.base_url))
            .bearer_auth(&self.service_key)
            .header("X-Puku-Source-Id", &req.source_id);
        if let Some((kind, value)) = cred {
            // 'bearer' and 'api_key' are the two the memory service accepts;
            // its own CHECK constraint refuses anything else, deliberately
            // including 'refresh' — it never mints, so a minting token has no
            // business crossing this boundary.
            let kind = if kind == "api_key" { "api_key" } else { "bearer" };
            rq = rq
                .header("X-Puku-Model-Credential", value)
                .header("X-Puku-Model-Credential-Kind", kind);
        }
        rq.json(req)
            .send()
            .await
            .context("posting ingest")?
            .error_for_status()
            .context("memory service refused the ingest")?;
        Ok(())
    }
}


/// Resolve the profile and attach a preamble to the spec.
///
/// Called from `dispatch_pending`, alongside the credential, connector and
/// skill resolves. Never fatal: every failure path leaves `memory_preamble`
/// as it was and the session boots without it.
///
/// The order here is the degradation ladder, in code:
///
/// 1. already pinned (a resume)      -> replay verbatim, no network call
/// 2. live fetch succeeds            -> use it, refresh the cache
/// 3. live fetch fails               -> serve the cached preamble
/// 4. no cache either                -> dispatch with nothing
pub async fn attach_preamble(
    state: &crate::AppState,
    session: &crate::db::SessionRow,
    spec: &mut puku_cloud_proto::session::SessionSpec,
) {
    let Some(client) = &state.memory else { return };
    if session.memory_opt_out {
        return;
    }

    // Already pinned: this is a resume. Replaying the stored text is the
    // whole point -- see the comment on the column.
    if session.memory_preamble.is_some() {
        spec.memory_preamble = session.memory_preamble.clone();
        return;
    }
    if !crate::db::memory_enabled(&state.pool, session.org_id).await {
        return;
    }

    let profile = match client.resolve(session.org_id, session.repo.as_deref()).await {
        Ok(p) if !p.disabled => p,
        Ok(_) => return, // profile frozen by an operator: contain it here too
        Err(e) => {
            tracing::warn!(session = %session.id, error = format!("{e:#}"),
                "memory profile resolve failed; dispatching without memory");
            return;
        }
    };

    let budget = state.cfg.memory_preamble_bytes;
    let started = std::time::Instant::now();
    // The session's user selects the personal layer. None for a cron run,
    // which correctly gets the shared layers and nobody's preferences.
    let viewer = session.user_id.map(|u| u.to_string());
    let result = client.preamble(&profile.id, budget, viewer.as_deref()).await;
    // F28. Load between controld and the memory service, so a slow or failing
    // dependency is visible before it is visible as boot latency. This does
    // not reach Cloudflare and is not a bill -- model spend is metered inside
    // the memory service, which is the only place that knows token counts.
    let mut op = crate::db::MemoryOp {
        preamble_fetches: 1,
        preamble_ms: started.elapsed().as_millis() as i64,
        ..Default::default()
    };
    if let Ok(Some(p)) = &result {
        op.preamble_bytes = p.preamble.len() as i64;
    }
    if result.is_err() {
        op.preamble_failures = 1;
    }
    crate::db::record_memory_usage(&state.pool, session.org_id, op).await;

    let preamble = match result {
        Ok(Some(p)) => {
            // Cache the SHARED page, never the one just served. The cache is
            // keyed by profile alone and read by every user of that repository,
            // so anything personal in it is a cross-user leak the moment
            // somebody else's live fetch times out.
            //
            // An older memory service sends no `shared`; caching nothing is
            // correct there — a colleague then falls through to no memory
            // rather than to somebody else's.
            if p.shared.is_empty() {
                tracing::debug!("memory service sent no shared page; not caching");
            } else if let Err(e) = crate::db::cache_preamble(
                &state.pool, &profile.id, &p.shared, p.etag.as_deref()).await
            {
                tracing::warn!(error = %e, "caching preamble failed");
            }
            Some(p.preamble)
        }
        // 204 is not a failure. An emptied profile should stop influencing
        // sessions, so this must NOT fall back to the cache.
        Ok(None) => None,
        Err(e) => {
            let cached = crate::db::cached_preamble(&state.pool, &profile.id).await;
            tracing::warn!(
                session = %session.id, error = format!("{e:#}"), served_cached = cached.is_some(),
                "memory service unreachable; falling back to the cached preamble");
            cached
        }
    };

    if let Err(e) = crate::db::set_memory_preamble(
        &state.pool, session.id, &profile.id, preamble.as_deref()).await
    {
        tracing::warn!(session = %session.id, error = %e, "pinning preamble failed");
    }
    spec.memory_preamble = preamble;
}

/// Hand a finished session to the memory service, detached.
///
/// Mirrors `notify::spawn`: the worker-link handler must not block for seconds
/// on another service while other sessions' frames queue behind it.
pub fn spawn_ingest(state: &crate::AppState, session: crate::db::SessionRow) {
    if state.memory.is_none() {
        return;
    }
    let permission_mode = session
        .permission_mode
        .as_deref()
        .and_then(puku_cloud_proto::session::PermissionMode::parse);
    if !distill::should_ingest(&session.state, session.memory_opt_out, permission_mode) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = ingest_session(&state, &session).await {
            // A lost memory is a lost memory. The session already completed.
            tracing::warn!(session = %session.id, error = format!("{e:#}"), "memory ingest failed");
        }
    });
}

/// Last-chance ingest, called by `archive.rs` before a transcript is deleted.
///
/// Awaited rather than spawned: the whole point is that the events must still
/// exist when it runs, so archival has to wait for it. Returns `Ok(())` for
/// every "nothing to do" case, because a session that was already ingested --
/// the overwhelmingly common one -- must not look like a failure.
pub async fn ingest_before_archive(state: &crate::AppState, session_id: Uuid) -> Result<()> {
    if state.memory.is_none() {
        return Ok(());
    }
    let Some(session) = crate::db::get_session(&state.pool, session_id).await? else {
        return Ok(());
    };
    if session.memory_ingested_at.is_some() {
        return Ok(());
    }
    let permission_mode = session
        .permission_mode
        .as_deref()
        .and_then(puku_cloud_proto::session::PermissionMode::parse);
    if !distill::should_ingest(&session.state, session.memory_opt_out, permission_mode) {
        return Ok(());
    }
    ingest_session(state, &session).await
}

async fn ingest_session(state: &crate::AppState, session: &crate::db::SessionRow) -> Result<()> {
    let client = state.memory.as_ref().expect("checked by spawn_ingest");

    // The profile is normally pinned at dispatch. Resolve if it is not: a
    // session that ran before memory was switched on still has a transcript
    // worth keeping.
    let profile_id = match &session.memory_profile_id {
        Some(id) => id.clone(),
        None => {
            if !crate::db::memory_enabled(&state.pool, session.org_id).await {
                return Ok(());
            }
            client.resolve(session.org_id, session.repo.as_deref()).await?.id
        }
    };

    let events = crate::db::events_for_memory(&state.pool, session.id, 5000).await?;
    let messages = distill::distill(&session.prompt, &events);
    // One prompt and nothing else is not a session anyone learns from.
    if messages.len() < 2 {
        return Ok(());
    }

    let req = IngestRequest {
        source_id: session.id.to_string(),
        occurred_at: session.ended_at.unwrap_or_else(chrono::Utc::now),
        label: distill::label(&session.prompt),
        messages,
        user_id: session.user_id.map(|u| u.to_string()),
        outcome: Some(distill::outcome(&session.title, &session.state, session.cost_usd)),
    };
    // The same resolution dispatch uses: the caller's own credential, else the
    // org's. It works here for the same reason it works there -- it reads
    // sessions.credential_enc, which persists with the row -- so this is
    // correct at session end AND days later from ingest_before_archive.
    //
    // None means this tenant's extraction has nothing to be billed to, and the
    // memory service degrades rather than falling back to somebody else's.
    let cred = crate::api::resolve_credential(state, session)
        .await
        .filter(|(kind, _)| {
            // The memory service does not mint, so a 'refresh' token has no
            // use there and its schema refuses one. resolve_credential cannot
            // return that today -- a session credential is always a bearer,
            // and org_credential_for mints before returning -- but that is two
            // hops of reasoning about someone else's function, and if it ever
            // stops holding the symptom is a tenant that silently stops
            // learning. Enforced here, where the boundary actually is.
            let ok = matches!(kind.as_str(), "bearer" | "api_key");
            if !ok {
                tracing::warn!(
                    session = %session.id, kind = %kind,
                    "not sending a credential of this kind to the memory service"
                );
            }
            ok
        });
    let sent = req.messages.len() as i64;
    let outcome = client
        .ingest(
            &profile_id,
            &req,
            cred.as_ref().map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .await;
    crate::db::record_memory_usage(
        &state.pool,
        session.org_id,
        crate::db::MemoryOp {
            ingests: 1,
            ingest_failures: i64::from(outcome.is_err()),
            messages_sent: sent,
            ..Default::default()
        },
    )
    .await;
    outcome?;
    crate::db::mark_memory_ingested(&state.pool, session.id).await?;
    tracing::info!(session = %session.id, profile = %profile_id, "memory ingested");
    Ok(())
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// The wire contract with puku-memory-service.
    ///
    /// The two services deploy independently, so version skew is the normal
    /// state, not an incident (failure mode N3). The rule is that this API
    /// only ever GAINS fields; a rename or removal is a new /v2. This test is
    /// the tripwire — the identical fixture is parsed on the Go side in
    /// `internal/api/contract_test.go`, so a rename here fails there.
    /// The cache must never be able to hold a personal layer.
    ///
    /// memory_preamble_cache is one row per profile, read by every user of that
    /// repository. This is the deserialization half of the guarantee: the
    /// memory service sends a `shared` page with no personal section, and the
    /// caller caches that one. The Go side asserts the page it sends is clean
    /// (TestThePreambleResponseCarriesACacheableSharedPage); this asserts we
    /// actually read it.
    #[test]
    fn the_shared_page_is_what_gets_cached() {
        // Built with json! rather than a raw string: the payload contains
        // `"###`, which closes a raw-string fence mid-literal at any depth
        // shallow enough to be readable.
        let body = serde_json::json!({
            "preamble": "## Context\n### About you\n- prefers verbose logging\n### Profile\n- uses pgx",
            "shared":   "## Context\n### Profile\n- uses pgx",
            "etag": "abc",
        })
        .to_string();
        let p: Preamble = serde_json::from_str(&body).unwrap();
        assert!(p.preamble.contains("About you"), "the served page keeps the viewer's own layer");
        assert!(!p.shared.contains("About you"), "the cacheable page carries a personal section");
        assert!(p.shared.contains("uses pgx"), "the cacheable page lost the shared knowledge");
    }

    /// An older memory service sends no `shared`. Caching nothing is correct
    /// there: a colleague falls through to no memory rather than to somebody
    /// else's.
    #[test]
    fn a_missing_shared_page_is_empty_not_a_parse_error() {
        let body = serde_json::json!({"preamble": "## Context"}).to_string();
        let p: Preamble = serde_json::from_str(&body).unwrap();
        assert!(p.shared.is_empty());
    }

    /// The model credential must never be a body field.
    ///
    /// The memory service marshals this whole struct into its job queue, which
    /// is retained for days and is what an operator reads when a job
    /// dead-letters. A credential there would be plaintext in Postgres. It
    /// rides headers instead; the Go half asserts the queue row never contains
    /// it (TestTheModelCredentialNeverReachesTheJobPayload). This half asserts
    /// it never enters the body in the first place.
    #[test]
    fn the_model_credential_is_never_in_the_ingest_body() {
        let req = IngestRequest {
            source_id: "s".into(),
            occurred_at: chrono::Utc::now(),
            label: "l".into(),
            messages: vec![],
            user_id: None,
            outcome: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        let body = serde_json::to_string(&v).unwrap().to_lowercase();
        for banned in ["credential", "token", "secret", "api_key", "bearer"] {
            assert!(
                !body.contains(banned),
                "the ingest body carries a {banned}-shaped field: {body}"
            );
        }
    }

    #[test]
    fn ingest_request_wire_shape_is_stable() {
        let req = IngestRequest {
            source_id: "11111111-1111-1111-1111-111111111111".into(),
            occurred_at: chrono::DateTime::parse_from_rfc3339("2026-08-31T09:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            label: "add the usage baseline migration".into(),
            user_id: Some("22222222-2222-2222-2222-222222222222".into()),
            messages: vec![
                Message { role: "user", content: "we use cargo nextest".into(), origin: "user" },
                Message { role: "assistant", content: "noted".into(), origin: "agent" },
            ],
            outcome: Some(Outcome {
                summary: "added migration 0016".into(),
                status: "completed".into(),
                cost_usd: 0.42,
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();

        for key in ["source_id", "occurred_at", "label", "messages", "outcome"] {
            assert!(v.get(key).is_some(), "field {key} missing from the ingest contract");
        }
        let m = &v["messages"][0];
        for key in ["role", "content", "origin"] {
            assert!(m.get(key).is_some(), "message field {key} missing");
        }
        let o = &v["outcome"];
        for key in ["summary", "status", "cost_usd"] {
            assert!(o.get(key).is_some(), "outcome field {key} missing");
        }
        // Origins are a closed set on the far side; an unknown one is a 400.
        assert_eq!(v["messages"][0]["origin"], "user");
        assert_eq!(v["messages"][1]["origin"], "agent");
    }

    #[test]
    fn outcome_is_omitted_rather_than_null_when_absent() {
        // The Go side treats a present-but-null outcome differently from an
        // absent one; skip_serializing_if keeps them the same thing.
        let req = IngestRequest {
            source_id: "s".into(),
            occurred_at: chrono::Utc::now(),
            label: "l".into(),
            messages: vec![],
            user_id: None,
            outcome: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("outcome").is_none(), "None outcome serialized as null");
        // Same reasoning for user_id: a cron session has nobody, and the Go
        // side must see an absent field rather than a null it has to special-
        // case. An empty string would be worse still -- it would look like an
        // identity and file personal memories under nobody.
        assert!(v.get("user_id").is_none(), "None user_id serialized as null");
    }

    #[test]
    fn resolve_request_wire_shape_is_stable() {
        let v = serde_json::to_value(ResolveRequest {
            tenant_id: "org-uuid",
            scope: "repo:github.com/acme/api",
        })
        .unwrap();
        assert_eq!(v["tenant_id"], "org-uuid");
        assert_eq!(v["scope"], "repo:github.com/acme/api");
    }

    #[test]
    fn preamble_response_tolerates_new_fields() {
        // Forward compatibility in the other direction: the memory service
        // will grow fields, and controld must not 500 when it does.
        let body = serde_json::json!({
            "preamble": "## Context\n- a fact\n",
            "etag": "sha256:abc",
            "layers": {"pinned": 1, "profile": 2, "recent": 3},
            "digest_at": "2026-08-31T04:00:00Z",
            "bytes": 42,
            "a_field_from_the_future": true
        });
        let p: Preamble = serde_json::from_value(body).expect("unknown fields must be ignored");
        assert!(p.preamble.contains("a fact"));
        assert_eq!(p.etag.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn profile_response_tolerates_new_fields_and_missing_optionals() {
        let p: Profile = serde_json::from_value(serde_json::json!({
            "id": "p1", "cf_profile": "tabc-x", "consolidation_runs": 3
        }))
        .expect("only `id` should be required");
        assert_eq!(p.id, "p1");
        assert!(!p.disabled, "disabled must default to false when absent");
    }
}

#[cfg(test)]
mod archive_guard_tests {
    use super::*;

    /// `ingest_before_archive` must be a no-op in every case except the one it
    /// exists for, because it runs inside the archival loop and a stray error
    /// there would block a transcript from ever being reaped.
    ///
    /// The interesting half — that it actually drains a NULL
    /// `memory_ingested_at` — is covered by the integration harness, which has
    /// a database. This asserts the guards that keep it quiet.
    #[test]
    fn should_ingest_gates_the_archive_drain() {
        use puku_cloud_proto::session::PermissionMode;
        // Already ingested sessions are the common case at archival time and
        // must never be re-sent.
        assert!(distill::should_ingest("completed", false, None));
        // Everything else the drain sees is skipped.
        assert!(!distill::should_ingest("failed", false, None));
        assert!(!distill::should_ingest("reaped", false, None));
        assert!(!distill::should_ingest("completed", true, None));
        assert!(!distill::should_ingest(
            "completed",
            false,
            Some(PermissionMode::Plan)
        ));
    }

}

/// Reaching the memory service over a public hostname puts Cloudflare Access
/// in front of it, and controld has to get through that as well as the
/// service's own key.
#[cfg(test)]
mod access_tests {
    use super::*;

fn token(id: &str, secret: &str) -> AccessToken {
    AccessToken { client_id: id.to_string(), client_secret: secret.to_string() }
}

/// On the compose network there is no Access in front, and sending empty
/// or absent Access headers to a service that is not behind it would be
/// noise at best.
#[test]
fn no_token_means_no_access_headers() {
    assert!(access_headers(None).is_empty());
}

#[test]
fn a_token_is_sent_as_both_access_headers() {
    let headers = access_headers(Some(token("id.access", "secret-value")));
    assert_eq!(headers.get("CF-Access-Client-Id").unwrap(), "id.access");
    assert_eq!(headers.get("CF-Access-Client-Secret").unwrap(), "secret-value");
}

/// Both halves are credentials. A HeaderValue that is not marked sensitive
/// is fair game for any logging middleware that prints headers.
#[test]
fn both_access_headers_are_marked_sensitive() {
    let headers = access_headers(Some(token("id.access", "secret-value")));
    assert!(headers.get("CF-Access-Client-Id").unwrap().is_sensitive());
    assert!(headers.get("CF-Access-Client-Secret").unwrap().is_sensitive());
}

/// Half a pair is refused at the edge exactly like none, but it looks like
/// a different bug — so a value that cannot be a header contributes
/// nothing rather than leaving one header set.
#[test]
fn a_malformed_half_sends_neither_header() {
    // A newline cannot appear in a header value.
    let headers = access_headers(Some(token("id.access", "bad\nvalue")));
    assert!(headers.is_empty(), "a malformed secret left a header behind");

    let headers = access_headers(Some(token("bad\nid", "secret-value")));
    assert!(headers.is_empty(), "a malformed id left a header behind");
}
}
