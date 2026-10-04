//! Brokered connectors, via the MCP proxy the ecosystem already runs.
//!
//! `puku-cowork` solved this for local VM sessions: the agent gets an MCP
//! endpoint at `mcp.proxy.puku.sh/v1/mcp/<id>` plus the user's puku JWT, and
//! the proxy swaps that for the vendor's OAuth token server-side
//! (`app/src/main/connectors.ts`). Cloud sessions reuse it verbatim rather
//! than growing a second connector gateway.
//!
//! The security property worth stating: a microVM runs model-authored code,
//! and a prompt injection from a fetched web page is a live threat. A Gmail
//! refresh token inside that VM would be an exfiltration channel. A puku JWT
//! scoped to the session is not — it only opens the proxy, which enforces
//! what the user actually connected.

use std::time::Duration;

use puku_cloud_proto::session::McpServerSpec;

/// The proxy's list of *connected* servers for this user. Note
/// `/v1/mcp_servers` (what the user connected), not `/v1/connectors` (the
/// catalog of what they could connect).
const LIST_PATH: &str = "/v1/mcp_servers";

#[derive(serde::Deserialize)]
struct ListResponse {
    #[serde(default)]
    data: Vec<ServerRef>,
}

#[derive(serde::Deserialize)]
struct ServerRef {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Clone)]
pub struct ConnectorClient {
    proxy_url: String,
    http: reqwest::Client,
}

impl ConnectorClient {
    pub fn new(proxy_url: String) -> Self {
        ConnectorClient {
            proxy_url: proxy_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                // A slow proxy must not hold up dispatch: a session without
                // its connectors is far better than a session that never
                // boots.
                .timeout(Duration::from_secs(5))
                .build()
                .expect("building the http client"),
        }
    }

    pub fn proxy_url(&self) -> &str {
        &self.proxy_url
    }

    /// MCP entries for everything this user has connected. Returns an empty
    /// list on any failure — connectors are additive capability, never a
    /// reason to fail a session.
    pub async fn servers_for(&self, bearer: &str) -> Vec<McpServerSpec> {
        let url = format!("{}{LIST_PATH}", self.proxy_url);
        let resp = match self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%url, error = %e, "listing connectors failed");
                return Vec::new();
            }
        };
        if !resp.status().is_success() {
            tracing::warn!(%url, status = %resp.status(), "connector proxy refused the list");
            return Vec::new();
        }
        match resp.json::<ListResponse>().await {
            Ok(list) => list
                .data
                .into_iter()
                .map(|s| {
                    let display = s.display_name.clone().unwrap_or_else(|| s.id.clone());
                    McpServerSpec::connector(&self.proxy_url, &s.id, &display)
                })
                .collect(),
            Err(e) => {
                tracing::warn!(%url, error = %e, "connector list was not the expected shape");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matches the response puku-cowork's `fetchConnectors` parses:
    /// `{data: [{id, display_name}]}` from `/v1/mcp_servers`.
    #[test]
    fn parses_the_proxy_list_shape() {
        let body = serde_json::json!({
            "data": [
                {"id": "slack", "display_name": "Slack"},
                {"id": "github", "display_name": "GitHub"},
            ]
        });
        let list: ListResponse = serde_json::from_value(body).unwrap();
        assert_eq!(list.data.len(), 2);
        assert_eq!(list.data[0].id, "slack");
    }

    /// A server with no display_name must still produce a usable entry
    /// rather than being dropped.
    #[test]
    fn tolerates_a_missing_display_name() {
        let list: ListResponse =
            serde_json::from_value(serde_json::json!({"data": [{"id": "notion"}]})).unwrap();
        let s = &list.data[0];
        let entry = McpServerSpec::connector(
            "https://mcp.proxy.puku.sh",
            &s.id,
            s.display_name.as_deref().unwrap_or(&s.id),
        );
        assert_eq!(entry.name, "puku.ai notion");
    }

    /// An empty or absent `data` is a user with nothing connected, not an
    /// error.
    #[test]
    fn an_empty_list_is_not_an_error() {
        let list: ListResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(list.data.is_empty());
    }
}
