//! Minting a short-lived bearer from a stored refresh token.
//!
//! Unattended runs are the reason this exists. A scheduled job fires when
//! nobody is holding a request, so it needs a credential stored in advance.
//! Storing an access token means every schedule stops working the day its
//! owner's login lapses, silently, with a 401 that reads like a revoked key.
//! Storing a long-lived API key means asking each user to mint and rotate
//! one by hand.
//!
//! A refresh token is the third option: durable, and it yields a fresh
//! access token whenever one is needed. The refresh token stays in the
//! control plane -- the guest only ever receives the short-lived bearer, so
//! the boundary that `connectors.rs` draws around the VM still holds.
//!
//! The wire format is openauth's, which is what puku-cli uses:
//!   POST {issuer}/token
//!   Content-Type: application/x-www-form-urlencoded
//!   grant_type=refresh_token&refresh_token=...
//! answering `{access_token, refresh_token, expires_in}`. Issuers may rotate
//! the refresh token on each use, so the reply's is stored when present.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A freshly minted access token, and the refresh token to store for next
/// time (which may or may not be the one that was sent).
#[derive(Debug, Clone)]
pub struct Minted {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Lifetime in seconds, when the issuer states one.
    pub expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

/// Exchange a refresh token for an access token.
///
/// A 4xx means the refresh token is spent or revoked and retrying will not
/// help; the caller should surface that to the session rather than loop.
pub async fn refresh(issuer: &str, refresh_token: &str) -> Result<Minted> {
    let url = format!("{}/token", issuer.trim_end_matches('/'));
    let res = reqwest::Client::new()
        .post(&url)
        .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token)])
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("posting to {url}"))?;

    let status = res.status();
    if !status.is_success() {
        // Deliberately not including the body: an issuer that echoes the
        // submitted token would put a live credential in the logs.
        bail!("refresh rejected by {url}: HTTP {status}");
    }
    let body: TokenResponse = res.json().await.context("decoding the token response")?;
    if body.access_token.trim().is_empty() {
        bail!("{url} returned an empty access_token");
    }
    Ok(Minted {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        expires_in: body.expires_in,
    })
}

/// When a minted bearer should be treated as stale.
///
/// Deliberately early: a token that expires mid-session is worse than one
/// minted a little too often, because the failure lands on the agent as a
/// 401 halfway through a turn rather than at dispatch.
pub fn expiry_from(expires_in: Option<i64>) -> Option<chrono::DateTime<chrono::Utc>> {
    let secs = expires_in?;
    let usable = (secs - 300).max(30);
    Some(chrono::Utc::now() + chrono::Duration::seconds(usable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_leaves_a_margin_before_the_real_one() {
        let e = expiry_from(Some(3600)).unwrap();
        let secs = (e - chrono::Utc::now()).num_seconds();
        assert!((3200..3350).contains(&secs), "expected ~55min, got {secs}s");
    }

    /// A short-lived token must still be usable rather than pre-expired.
    #[test]
    fn a_short_lifetime_does_not_go_negative() {
        let e = expiry_from(Some(60)).unwrap();
        assert!(e > chrono::Utc::now(), "a 60s token must not be born stale");
    }

    #[test]
    fn no_stated_lifetime_means_no_cached_expiry() {
        assert!(expiry_from(None).is_none());
    }
}
