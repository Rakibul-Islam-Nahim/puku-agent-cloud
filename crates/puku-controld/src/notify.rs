//! Reaching the human when a session needs them.
//!
//! Missing this made the cron scheduler decorative: a scheduled session
//! that hits `waiting_input` at 03:00 waits until someone happens to open
//! the dashboard. "Always-on"
//! is worthless without an interrupt path back to a person.
//!
//! Delivery is best-effort and off the request path — a webhook that hangs
//! must never hold up a state transition.

use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::SessionRow;
use crate::AppState;

/// The two moments a human cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The agent is blocked on a question and will wait forever.
    WaitingInput,
    /// The run is over (completed, failed or canceled).
    Terminal,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Trigger::WaitingInput => "waiting_input",
            Trigger::Terminal => "terminal",
        }
    }
}

#[derive(sqlx::FromRow)]
struct Target {
    id: Uuid,
    kind: String,
    config: serde_json::Value,
    secret_enc: Option<Vec<u8>>,
}

/// Fire notifications for a session that just changed state. Spawns and
/// returns immediately.
pub fn spawn(state: &AppState, session: SessionRow, trigger: Trigger) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = deliver(&state, &session, trigger).await {
            tracing::warn!(session = %session.id, error = format!("{e:#}"), "notification failed");
        }
    });
}

/// What distinguishes two notifications for the same session and trigger.
/// Two different questions in one session are two events; the same question
/// re-observed (a worker reconnect replaying state) is one.
fn dedup_key(session: &SessionRow, trigger: Trigger) -> String {
    match trigger {
        Trigger::WaitingInput => session
            .pending_question
            .as_ref()
            .and_then(|q| q.get("request_id"))
            .and_then(|r| r.as_str())
            .unwrap_or("pending")
            .to_string(),
        Trigger::Terminal => session.state.clone(),
    }
}

async fn deliver(state: &AppState, session: &SessionRow, trigger: Trigger) -> Result<()> {
    let targets: Vec<Target> = sqlx::query_as(
        "SELECT id, kind, config, secret_enc FROM notification_targets \
         WHERE org_id = $1 AND enabled AND $2 = ANY(events) \
           AND (user_id IS NULL OR user_id IS NOT DISTINCT FROM $3)",
    )
    .bind(session.org_id)
    .bind(trigger.as_str())
    .bind(session.user_id)
    .fetch_all(&state.pool)
    .await?;
    if targets.is_empty() {
        return Ok(());
    }

    let key = dedup_key(session, trigger);
    let payload = payload_for(session, trigger);
    for target in targets {
        // Claim before sending: the unique key means a duplicate transition
        // (worker reconnect, retry) can't page the user twice.
        let claimed = sqlx::query(
            "INSERT INTO notification_deliveries (target_id, session_id, event, dedup_key) \
             VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(target.id)
        .bind(session.id)
        .bind(trigger.as_str())
        .bind(&key)
        .execute(&state.pool)
        .await?;
        if claimed.rows_affected() == 0 {
            continue;
        }
        if let Err(e) = send_one(state, &target, &payload).await {
            tracing::warn!(target = %target.id, error = format!("{e:#}"), "notification delivery failed");
            // Release the claim so a later attempt can retry.
            sqlx::query(
                "DELETE FROM notification_deliveries \
                 WHERE target_id = $1 AND session_id = $2 AND event = $3 AND dedup_key = $4",
            )
            .bind(target.id)
            .bind(session.id)
            .bind(trigger.as_str())
            .bind(&key)
            .execute(&state.pool)
            .await
            .ok();
        }
    }
    Ok(())
}

fn payload_for(session: &SessionRow, trigger: Trigger) -> serde_json::Value {
    json!({
        "event": trigger.as_str(),
        "session": {
            "id": session.id,
            "title": session.title,
            "state": session.state,
            "repo": session.repo,
            "cost_usd": session.cost_usd,
            "error": session.error,
        },
        "question": session.pending_question,
    })
}

/// Human-readable one-liner for chat targets.
fn summary(payload: &serde_json::Value) -> String {
    let s = &payload["session"];
    let title = s["title"].as_str().unwrap_or("(untitled)");
    let id = s["id"].as_str().unwrap_or("");
    match payload["event"].as_str() {
        Some("waiting_input") => {
            let q = payload["question"]["input"]["questions"][0]["question"]
                .as_str()
                .unwrap_or("The agent needs your input.");
            format!("*{title}* is waiting on you: {q}\nsession `{id}`")
        }
        _ => {
            let state = s["state"].as_str().unwrap_or("finished");
            format!("*{title}* {state}\nsession `{id}`")
        }
    }
}

async fn send_one(state: &AppState, target: &Target, payload: &serde_json::Value) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    match target.kind.as_str() {
        "webhook" => {
            let url = target
                .config
                .get("url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| anyhow::anyhow!("webhook target has no url"))?;
            let body = serde_json::to_vec(payload)?;
            let mut req = http.post(url).header("content-type", "application/json");
            // Signed so a receiver can tell a real callback from anyone who
            // learned the URL.
            if let (Some(enc), Some(secrets)) = (&target.secret_enc, &state.secrets) {
                if let Ok(secret) = secrets.decrypt(enc) {
                    req = req.header("x-puku-signature", sign(&secret, &body));
                }
            }
            let resp = req.body(body).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("webhook returned {}", resp.status());
            }
        }
        "slack" => {
            let url = target
                .config
                .get("url")
                .and_then(|u| u.as_str())
                .ok_or_else(|| anyhow::anyhow!("slack target has no url"))?;
            let resp = http
                .post(url)
                .json(&json!({"text": summary(payload)}))
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("slack webhook returned {}", resp.status());
            }
        }
        "platform" => {
            // Desktop/mobile push routes through the puku platform, which
            // owns the device registrations. The endpoint does not exist
            // yet; keep the target shape so enabling it later is config,
            // not a migration.
            tracing::debug!("platform push target is not wired up yet; skipping");
        }
        other => anyhow::bail!("unknown notification kind {other}"),
    }
    Ok(())
}

/// HMAC-SHA256, hex, prefixed with the scheme so the format can change later.
fn sign(secret: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    // Hand-rolled HMAC over the sha2 already in the dependency tree.
    const BLOCK: usize = 64;
    let mut key = secret.as_bytes().to_vec();
    if key.len() > BLOCK {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(BLOCK, 0);
    let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::digest([ipad.as_slice(), body].concat());
    let outer = Sha256::digest([opad.as_slice(), inner.as_slice()].concat());
    format!("sha256={}", hex::encode(outer))
}

/// Called from the state-transition path.
pub async fn on_transition(state: &AppState, session_id: Uuid, new_state: &str) {
    let trigger = match new_state {
        "waiting_input" => Trigger::WaitingInput,
        "completed" | "failed" | "canceled" => Trigger::Terminal,
        _ => return,
    };
    if let Ok(Some(session)) = crate::db::get_session(&state.pool, session_id).await {
        spawn(state, session, trigger);
    }
}

/// Housekeeping: delivery rows exist to dedup live traffic, not as history.
pub async fn prune(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM notification_deliveries WHERE sent_at < now() - interval '30 days'")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vector from RFC 4231 (case 2), proving the hand-rolled HMAC is
    /// really HMAC-SHA256 and not something that merely looks like it.
    #[test]
    fn hmac_matches_the_rfc_vector() {
        assert_eq!(
            sign("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signature_changes_with_the_body_and_the_secret() {
        assert_ne!(sign("k", b"a"), sign("k", b"b"));
        assert_ne!(sign("k1", b"a"), sign("k2", b"a"));
    }

    #[test]
    fn waiting_input_summary_leads_with_the_question() {
        let payload = json!({
            "event": "waiting_input",
            "session": {"id": "abc", "title": "fix the flaky test", "state": "waiting_input"},
            "question": {"input": {"questions": [{"question": "Retry or skip?"}]}},
        });
        let s = summary(&payload);
        assert!(s.contains("fix the flaky test"), "{s}");
        assert!(s.contains("Retry or skip?"), "{s}");
    }

    #[test]
    fn terminal_summary_names_the_end_state() {
        let payload = json!({
            "event": "terminal",
            "session": {"id": "abc", "title": "nightly audit", "state": "failed"},
        });
        let s = summary(&payload);
        assert!(s.contains("nightly audit") && s.contains("failed"), "{s}");
    }
}
