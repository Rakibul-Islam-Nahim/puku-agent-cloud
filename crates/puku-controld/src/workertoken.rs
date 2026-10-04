//! Per-worker registration credentials.
//!
//! Workers used to present one shared secret (`/etc/puku/worker-token`), so
//! any host holding it could register and be handed another org's sessions,
//! and revoking one compromised box meant rotating every box. A token here
//! is minted per host, stored as sha256, and **bound to the first worker
//! name that uses it** — a leaked token cannot silently fan out across
//! hosts, because the second name presenting it is refused.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::hash_key;

/// `pkw_` (puku cloud worker) so a leaked token is greppable and visibly
/// distinct from a client `pkc_` key.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("pkw_{}", hex::encode(bytes))
}

/// Mint a token for `name` and return the plaintext — the only time it is
/// ever visible.
pub async fn create(pool: &PgPool, name: &str) -> Result<String> {
    let token = generate_token();
    let hash = hash_key(&token);
    sqlx::query(
        "INSERT INTO worker_tokens (id, name, token_hash, prefix) VALUES ($1,$2,$3,$4)",
    )
    .bind(Uuid::new_v4())
    .bind(name)
    .bind(&hash)
    .bind(&token[..12])
    .execute(pool)
    .await
    .context("inserting worker token")?;
    Ok(token)
}

/// Why a registration was refused. Distinct variants so the log line says
/// which of the three failure modes happened instead of "auth failed".
#[derive(Debug, PartialEq, Eq)]
pub enum AuthFailure {
    /// No live `worker_tokens` row matches, and it isn't the shared secret.
    Unknown,
    /// The token exists but was revoked.
    Revoked,
    /// The token is already bound to a different worker name.
    BoundElsewhere { bound_to: String },
}

pub enum WorkerAuth {
    /// Authenticated by a per-worker token.
    Token { token_id: Uuid },
    /// Authenticated by the legacy shared secret, which is only accepted
    /// while `allow_shared` is set.
    Shared,
}

/// Authenticate a `Register` frame.
///
/// The shared token is checked *after* the per-worker table so that a
/// deployment mid-migration prefers the stronger credential, and so a
/// per-worker token that happens to be revoked can't fall through to the
/// shared one.
pub async fn authenticate(
    pool: &PgPool,
    worker_name: &str,
    presented: &str,
    shared_token: &str,
    allow_shared: bool,
) -> Result<Result<WorkerAuth, AuthFailure>> {
    let hash = hash_key(presented);
    let row: Option<(Uuid, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT id, revoked_at FROM worker_tokens WHERE token_hash = $1")
            .bind(&hash)
            .fetch_optional(pool)
            .await?;

    if let Some((token_id, revoked_at)) = row {
        if revoked_at.is_some() {
            return Ok(Err(AuthFailure::Revoked));
        }
        // Bind-on-first-use. Any *other* worker already registered against
        // this token means the token is being reused across hosts.
        let bound: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM workers WHERE token_id = $1 AND name <> $2 LIMIT 1",
        )
        .bind(token_id)
        .bind(worker_name)
        .fetch_optional(pool)
        .await?;
        if let Some((bound_to,)) = bound {
            return Ok(Err(AuthFailure::BoundElsewhere { bound_to }));
        }
        sqlx::query("UPDATE worker_tokens SET last_seen_at = now() WHERE id = $1")
            .bind(token_id)
            .execute(pool)
            .await?;
        return Ok(Ok(WorkerAuth::Token { token_id }));
    }

    // Constant-time-ish equality is not the concern here (the shared token
    // is being retired); accepting it at all is, so it is gated.
    if allow_shared && presented == shared_token {
        return Ok(Ok(WorkerAuth::Shared));
    }
    Ok(Err(AuthFailure::Unknown))
}
