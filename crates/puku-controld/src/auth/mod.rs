//! API-key authentication. Keys look like `pkc_<40 hex>`; only the sha256
//! of the full key is stored. When auth is off (dev), every request runs as
//! the seeded dev org.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub mod platform;

use crate::AppState;

#[derive(Clone, Debug)]
pub struct AuthCtx {
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub admin: bool,
    /// The caller's own platform bearer, when they authenticated with one.
    /// Carried so a session can run on *their* credential instead of the
    /// operator's global key. Never logged; stored only encrypted.
    pub bearer: Option<String>,
}

pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

pub fn generate_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("pkc_{}", hex::encode(bytes))
}

/// Extract the presented key: Authorization: Bearer header, or `api_key`
/// query parameter (WebSocket clients can't always set headers).
fn presented_key(req: &Request) -> Option<String> {
    if let Some(h) = req.headers().get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = h.to_str() {
            if let Some(k) = s.strip_prefix("Bearer ") {
                return Some(k.trim().to_string());
            }
        }
    }
    let query = req.uri().query().unwrap_or("");
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("api_key=") {
            return Some(v.to_string());
        }
    }
    None
}

pub async fn middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if !state.cfg.auth_required {
        req.extensions_mut().insert(AuthCtx {
            org_id: state.cfg.dev_org,
            user_id: Some(state.cfg.dev_user),
            admin: true,
            bearer: None,
        });
        return next.run(req).await;
    }

    let Some(key) = presented_key(&req) else {
        return unauthorized("missing api key");
    };

    // Two schemes, chosen by prefix. `pkc_` keys stay for CI, scripts and
    // the operator dashboard; anything else is treated as a puku platform
    // bearer, which is what Puku Desktop and the web app actually hold.
    if !key.starts_with("pkc_") {
        return match &state.platform {
            Some(platform) => platform_auth(state.clone(), platform.clone(), key, req, next).await,
            None => unauthorized("platform auth is disabled; present a pkc_ api key"),
        };
    }

    let hash = hash_key(&key);
    /// (org_id, user_id, scopes)
    type KeyRow = (Uuid, Option<Uuid>, Vec<String>);
    let row: Result<Option<KeyRow>, _> = sqlx::query_as(
        "SELECT org_id, user_id, scopes FROM api_keys \
         WHERE key_hash = $1 AND revoked_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await;
    match row {
        Ok(Some((org_id, user_id, scopes))) => {
            let pool = state.pool.clone();
            let h2 = hash.clone();
            tokio::spawn(async move {
                let _ = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE key_hash = $1")
                    .bind(h2)
                    .execute(&pool)
                    .await;
            });
            req.extensions_mut().insert(AuthCtx {
                org_id,
                user_id,
                admin: scopes.iter().any(|s| s == "admin"),
                bearer: None,
            });
            // Tag once, here, and every event raised by the handler that
            // follows carries it -- including a panic. This is the whole
            // reason the Sentry layers sit outside this middleware.
            puku_observability::tag_org(org_id);
            next.run(req).await
        }
        Ok(None) => unauthorized("invalid api key"),
        Err(e) => {
            tracing::error!(error = %e, "auth lookup failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "auth lookup failed").into_response()
        }
    }
}

/// Verify a platform bearer and run the request as that user.
async fn platform_auth(
    state: AppState,
    platform: std::sync::Arc<platform::PlatformAuth>,
    token: String,
    mut req: Request,
    next: Next,
) -> Response {
    let user = match platform.verify(&token).await {
        Ok(u) => u,
        Err(platform::VerifyError::Invalid) => return unauthorized("invalid puku token"),
        Err(platform::VerifyError::Unavailable(e)) => {
            // Never fail open: a platform outage must not authenticate
            // everyone. 503 tells the client to retry.
            tracing::error!(error = %e, "platform auth unavailable");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": {"message": "authentication service unavailable"}
                })),
            )
                .into_response();
        }
    };
    let (user_id, org_id) = match platform::provision(&state.pool, &user).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "provisioning platform user failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "provisioning failed").into_response();
        }
    };
    req.extensions_mut().insert(AuthCtx {
        org_id,
        user_id: Some(user_id),
        // Platform users are never admins here: fleet operations stay on
        // an explicitly-scoped `pkc_` key held by the operator.
        admin: false,
        bearer: Some(token),
    });
    puku_observability::tag_org(org_id);
    next.run(req).await
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({"error": {"message": msg}})),
    )
        .into_response()
}
