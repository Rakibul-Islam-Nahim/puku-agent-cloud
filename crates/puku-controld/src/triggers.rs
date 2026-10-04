//! Inbound webhook triggers: a session started by something happening
//! elsewhere.
//!
//! `scheduler.rs` already proved the seam — a producer that calls
//! `db::create_session` and lets the normal dispatcher take it from there.
//! Cron is one producer; this is another, and a GitHub or Slack producer
//! later is the same shape again.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

use crate::auth::hash_key;
use crate::{db, AppState};

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TriggerRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub name: String,
    pub kind: String,
    pub prefix: String,
    pub prompt: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub enabled: bool,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub last_session_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

const COLS: &str = "id, org_id, user_id, name, kind, prefix, prompt, repo, branch, model, \
    max_budget_usd::float8 AS max_budget_usd, enabled, last_fired_at, last_session_id, created_at";

/// `pkt_` (puku cloud trigger): greppable, and visibly distinct from client
/// (`pkc_`) and worker (`pkw_`) credentials.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("pkt_{}", hex::encode(bytes))
}

pub struct NewTrigger {
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub name: String,
    pub prompt: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
}

/// Returns the row and the plaintext token — the only time it is visible.
pub async fn create(pool: &sqlx::PgPool, new: NewTrigger) -> Result<(TriggerRow, String)> {
    let token = generate_token();
    let row = sqlx::query_as::<_, TriggerRow>(&format!(
        "INSERT INTO triggers (id, org_id, user_id, name, token_hash, prefix, prompt, repo, \
         branch, model, max_budget_usd) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11::float8::numeric) RETURNING {COLS}"
    ))
    .bind(Uuid::new_v4())
    .bind(new.org_id)
    .bind(new.user_id)
    .bind(&new.name)
    .bind(hash_key(&token))
    .bind(&token[..12])
    .bind(&new.prompt)
    .bind(&new.repo)
    .bind(&new.branch)
    .bind(&new.model)
    .bind(new.max_budget_usd)
    .fetch_one(pool)
    .await?;
    Ok((row, token))
}

pub async fn list(pool: &sqlx::PgPool, org_id: Uuid) -> Result<Vec<TriggerRow>> {
    Ok(sqlx::query_as::<_, TriggerRow>(&format!(
        "SELECT {COLS} FROM triggers WHERE org_id = $1 ORDER BY created_at DESC"
    ))
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

pub async fn remove(pool: &sqlx::PgPool, id: Uuid, org_id: Uuid) -> Result<bool> {
    let n = sqlx::query("DELETE FROM triggers WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn by_token(pool: &sqlx::PgPool, token: &str) -> Result<Option<TriggerRow>> {
    Ok(sqlx::query_as::<_, TriggerRow>(&format!(
        "SELECT {COLS} FROM triggers WHERE token_hash = $1 AND enabled"
    ))
    .bind(hash_key(token))
    .fetch_optional(pool)
    .await?)
}

/// Substitute the delivered payload into the prompt.
///
/// `{{payload}}` inserts the whole body as pretty JSON; `{{payload.a.b}}`
/// pulls one field. An unresolved path becomes an empty string rather than
/// leaving a literal `{{...}}` in the prompt for the model to puzzle over.
///
/// The payload is untrusted input from whoever holds the URL, so it lands
/// in the prompt as *data* only — it can never widen the session's tool
/// policy, which is fixed by the trigger and the deployment ceiling.
pub fn render_prompt(template: &str, payload: &serde_json::Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // Unterminated: emit the rest verbatim rather than losing it.
            out.push_str(&rest[start..]);
            return out;
        };
        let expr = after[..end].trim();
        out.push_str(&resolve(expr, payload));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn resolve(expr: &str, payload: &serde_json::Value) -> String {
    let Some(path) = expr.strip_prefix("payload") else {
        return String::new();
    };
    let mut cur = payload;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        match cur.get(segment) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    match cur {
        serde_json::Value::String(s) => s.clone(),
        // Whole-object substitution is pretty-printed: a prompt is read by
        // a model, and one-line JSON is markedly harder for it to use.
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// Fire a trigger: create a session from its template and dispatch it.
pub async fn fire(
    state: &AppState,
    trigger: &TriggerRow,
    payload: &serde_json::Value,
) -> Result<Uuid> {
    if let Err(reason) = db::check_quota(&state.pool, trigger.org_id).await? {
        anyhow::bail!("over quota: {reason}");
    }
    let prompt = render_prompt(&trigger.prompt, payload);
    let session = db::create_session(
        &state.pool,
        db::NewSession {
            // A webhook-fired session has no caller to express a preference,
            // so it takes the org default like any other run.
            memory_opt_out: false,
            org_id: trigger.org_id,
            user_id: trigger.user_id,
            title: Some(if trigger.name.trim().is_empty() {
                db::title_from_prompt(&prompt)
            } else {
                trigger.name.clone()
            }),
            prompt,
            repo: trigger.repo.clone(),
            branch: trigger.branch.clone(),
            model: trigger.model.clone(),
            max_budget_usd: trigger.max_budget_usd,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            permission_mode: None,
            max_turns: None,
            // Unattended, like a cron run: no live caller to borrow a
            // bearer from, so the org's stored credential is used.
            credential: None,
            connectors: None,
            packs: Vec::new(),
            output_schema: None,
            idle_timeout_s: Some(180),
            max_duration_s: None,
            // Not a teleport: no transcript, and the id is generated.
            id: None,
            puku_session_id: None,
            import_ref: None,
            import_inline: None,
            // A webhook names nothing, so it runs where anything unnamed runs.
            engine: state.cfg.engine_default,
        },
    )
    .await?;
    sqlx::query("UPDATE triggers SET last_fired_at = now(), last_session_id = $2 WHERE id = $1")
        .bind(trigger.id)
        .bind(session.id)
        .execute(&state.pool)
        .await?;
    db::audit(
        &state.pool,
        Some(trigger.org_id),
        trigger.user_id,
        "trigger.fire",
        &trigger.id.to_string(),
        serde_json::json!({"session_id": session.id, "name": trigger.name}),
    )
    .await
    .ok();
    Ok(session.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_a_single_field() {
        let p = json!({"issue": {"title": "login is broken", "number": 42}});
        assert_eq!(
            render_prompt("Fix {{payload.issue.title}} (#{{payload.issue.number}})", &p),
            "Fix login is broken (#42)"
        );
    }

    /// Whole-payload substitution is pretty-printed: the model reads this.
    #[test]
    fn substitutes_the_whole_payload() {
        let out = render_prompt("Context:\n{{payload}}", &json!({"a": 1}));
        assert!(out.starts_with("Context:\n{"), "{out}");
        assert!(out.contains("\"a\": 1"), "{out}");
    }

    /// A missing field must not leave `{{payload.nope}}` in the prompt for
    /// the model to interpret as an instruction.
    #[test]
    fn unknown_paths_become_empty() {
        assert_eq!(render_prompt("x{{payload.nope.deeper}}y", &json!({})), "xy");
    }

    #[test]
    fn templates_without_placeholders_are_unchanged() {
        assert_eq!(render_prompt("just do the thing", &json!({})), "just do the thing");
    }

    /// Malformed templates must not silently truncate the prompt.
    #[test]
    fn an_unterminated_placeholder_keeps_the_text() {
        assert_eq!(render_prompt("do {{payload.x", &json!({})), "do {{payload.x");
    }

    #[test]
    fn tokens_are_distinguishable_from_other_credential_kinds() {
        let t = generate_token();
        assert!(t.starts_with("pkt_"), "{t}");
        assert_eq!(t.len(), 44);
        assert_ne!(generate_token(), generate_token());
    }
}
