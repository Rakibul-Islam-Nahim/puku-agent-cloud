//! Platform identity: verify a puku bearer against the platform API.
//!
//! Before this, controld was the only component in the puku ecosystem with
//! its own identity provider (`pkc_` keys, its own `users` table). Puku
//! Desktop and the web app hold a platform JWT, so they simply could not
//! call this API. Now they can, and a user's cloud sessions belong to their
//! puku account.
//!
//! The token is **never decoded locally** — `puku-chat-compute-service`
//! forwards it to the platform and trusts only the response, and doing the
//! same here means there is one place that knows how to validate a puku
//! token, not two that can disagree.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// How long a successful verification is trusted without re-asking. Short
/// enough that a revoked token stops working promptly, long enough that a
/// busy attach stream isn't one platform round trip per request.
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct PlatformUser {
    pub subject: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// sha256(token) -> (verified user, when it was verified).
type VerifyCache = Arc<RwLock<HashMap<Vec<u8>, (PlatformUser, Instant)>>>;

#[derive(Clone)]
pub struct PlatformAuth {
    api_url: String,
    http: reqwest::Client,
    /// Keyed by sha256 of the token: the cache never holds a bearer.
    cache: VerifyCache,
}

/// Distinguishes "this token is bad" (401) from "we could not tell" (503).
/// Failing open on a transport error would turn a platform outage into an
/// authentication bypass.
#[derive(Debug)]
pub enum VerifyError {
    Invalid,
    Unavailable(String),
}

impl PlatformAuth {
    pub fn new(api_url: String) -> Self {
        PlatformAuth {
            api_url: api_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building the http client"),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn cache_key(token: &str) -> Vec<u8> {
        Sha256::digest(token.as_bytes()).to_vec()
    }

    pub async fn verify(&self, token: &str) -> Result<PlatformUser, VerifyError> {
        let key = Self::cache_key(token);
        if let Some((user, at)) = self.cache.read().unwrap().get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(user.clone());
            }
        }

        let resp = self
            .http
            .get(format!("{}/auth/verify", self.api_url))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| VerifyError::Unavailable(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(VerifyError::Invalid);
        }
        if !resp.status().is_success() {
            return Err(VerifyError::Unavailable(format!(
                "auth/verify returned {}",
                resp.status()
            )));
        }
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| VerifyError::Unavailable(e.to_string()))?;
        let user = parse_verify_response(&payload).ok_or(VerifyError::Invalid)?;

        self.cache
            .write()
            .unwrap()
            .insert(key, (user.clone(), Instant::now()));
        Ok(user)
    }

    /// Drop expired entries. Called from the same periodic loop as the other
    /// housekeeping so a long-lived process doesn't accumulate every token
    /// it has ever seen.
    pub fn prune_cache(&self) {
        self.cache
            .write()
            .unwrap()
            .retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
    }
}

/// The platform's response shape, matched to what
/// `puku-chat-compute-service/app/auth.py` accepts: the subject may arrive
/// as `sub`, `user.sub`, `user.id` or `id`, and an explicit
/// `{"valid": false}` is a rejection even with HTTP 200.
fn parse_verify_response(payload: &serde_json::Value) -> Option<PlatformUser> {
    if payload.get("valid").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let user = payload.get("user");
    let str_at = |v: Option<&serde_json::Value>, k: &str| {
        v.and_then(|v| v.get(k)).and_then(|s| s.as_str()).map(str::to_string)
    };
    let subject = payload
        .get("sub")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .or_else(|| str_at(user, "sub"))
        .or_else(|| str_at(user, "id"))
        .or_else(|| payload.get("id").and_then(|s| s.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty())?;
    Some(PlatformUser {
        subject,
        email: str_at(user, "email")
            .or_else(|| payload.get("email").and_then(|s| s.as_str()).map(str::to_string)),
        name: str_at(user, "name")
            .or_else(|| payload.get("name").and_then(|s| s.as_str()).map(str::to_string)),
    })
}

/// Find or create the local projection of a platform user, plus the
/// personal org their sessions bill to.
///
/// The platform models no orgs — only `sub` — so one personal org per user
/// is synthesized here. Team orgs stay possible: `orgs.kind` distinguishes
/// them, and a user can later be moved without touching this path.
pub async fn provision(pool: &PgPool, user: &PlatformUser) -> Result<(Uuid, Uuid)> {
    if let Some((user_id, org_id)) =
        sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, org_id FROM users WHERE external_id = $1")
            .bind(&user.subject)
            .fetch_optional(pool)
            .await?
    {
        return Ok((user_id, org_id));
    }

    // One transaction: a half-provisioned user with no org (or an org with
    // no quota row) would fail every later request in a way that looks like
    // a bug rather than a race.
    let mut tx = pool.begin().await?;
    let user_id = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name, kind) VALUES ($1, $2, 'personal')")
        .bind(org_id)
        .bind(format!("user:{}", user.subject))
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, external_id, email, display_name, role) \
         VALUES ($1, $2, $3, $4, $5, 'owner')",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(&user.subject)
    .bind(&user.email)
    .bind(&user.name)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE orgs SET owner_user_id = $2 WHERE id = $1")
        .bind(org_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    // Without a quota row the org silently inherits the defaults in
    // check_quota; give it a real one so an operator can raise it.
    sqlx::query("INSERT INTO quotas (org_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await.context("provisioning platform user")?;

    tracing::info!(subject = %user.subject, %user_id, %org_id, "provisioned platform user");
    Ok((user_id, org_id))
}

#[cfg(test)]
mod tests {
    use super::parse_verify_response;
    use serde_json::json;

    /// The four subject shapes `app/auth.py` accepts, so a platform
    /// response that works for the compute service works here too.
    #[test]
    fn accepts_every_subject_shape_the_platform_uses() {
        for payload in [
            json!({"sub": "user_123"}),
            json!({"user": {"sub": "user_123"}}),
            json!({"user": {"id": "user_123"}}),
            json!({"id": "user_123"}),
        ] {
            let u = parse_verify_response(&payload)
                .unwrap_or_else(|| panic!("should parse {payload}"));
            assert_eq!(u.subject, "user_123");
        }
    }

    #[test]
    fn picks_up_profile_fields_when_present() {
        let u = parse_verify_response(&json!({
            "sub": "user_123",
            "user": {"email": "a@b.test", "name": "A B"},
        }))
        .unwrap();
        assert_eq!(u.email.as_deref(), Some("a@b.test"));
        assert_eq!(u.name.as_deref(), Some("A B"));
    }

    /// HTTP 200 with `valid: false` is a rejection, not a success.
    #[test]
    fn explicit_invalid_is_rejected_even_with_a_subject() {
        assert!(parse_verify_response(&json!({"valid": false, "sub": "user_123"})).is_none());
    }

    #[test]
    fn a_response_with_no_subject_is_rejected() {
        assert!(parse_verify_response(&json!({"valid": true})).is_none());
        assert!(parse_verify_response(&json!({"sub": ""})).is_none());
    }
}
