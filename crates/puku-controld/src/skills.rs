//! Client for puku-skills-service.
//!
//! Skills give an agent *competence* the way connectors give it *reach*,
//! and the shape is deliberately the same as `connectors.rs`: resolve at
//! dispatch with the caller's own bearer, put the result in the spec, let
//! the worker do the fetching.
//!
//! The registry is a separate service with its own database and deploy, so
//! this is a plain HTTP client — there is no shared library between them on
//! purpose.

use std::time::Duration;

use puku_cloud_proto::session::SkillPackRef;

#[derive(serde::Deserialize)]
struct ResolveResponse {
    #[serde(default)]
    packs: Vec<SkillPackRef>,
}

#[derive(Clone)]
pub struct SkillsClient {
    base_url: String,
    /// Service credential, used when there is no caller bearer to borrow —
    /// a scheduled run, a `pkc_` key, or a dev deployment with auth off.
    service_token: Option<String>,
    http: reqwest::Client,
}

impl SkillsClient {
    pub fn new(base_url: String, service_token: Option<String>) -> Self {
        SkillsClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            service_token,
            http: reqwest::Client::builder()
                // A slow registry must not hold up dispatch.
                .timeout(Duration::from_secs(10))
                .build()
                .expect("building the http client"),
        }
    }

    /// Resolve names (`office`, `bgp-debug@2.1`) to fetchable versions.
    ///
    /// An empty `packs` means "the org's defaults", which the registry
    /// decides. Errors are returned rather than swallowed: unlike
    /// connectors, a session told to use a skill it did not get will fail
    /// in a way that looks like the agent being stupid, so it is better to
    /// fail the dispatch with a clear reason.
    pub async fn resolve(
        &self,
        bearer: Option<&str>,
        org: uuid::Uuid,
        packs: &[String],
    ) -> anyhow::Result<Vec<SkillPackRef>> {
        let mut url = format!("{}/v1/resolve", self.base_url);
        if !packs.is_empty() {
            url.push_str(&format!("?packs={}", urlencode(&packs.join(","))));
        }
        // Prefer the caller's own bearer: the registry then sees exactly
        // what that user can see. Fall back to the service token, naming
        // the org, for runs with nobody behind them.
        let mut req = self.http.get(&url);
        req = match (bearer, &self.service_token) {
            (Some(b), _) => req.header(reqwest::header::AUTHORIZATION, format!("Bearer {b}")),
            (None, Some(svc)) => req
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {svc}"))
                .header("x-puku-org", org.to_string()),
            (None, None) => {
                anyhow::bail!(
                    "no credential for the skills registry: the session has no bearer and \
                     PUKU_SKILLS_TOKEN is unset"
                )
            }
        };
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("skills registry unreachable: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or(body);
            anyhow::bail!("skills registry returned {status}: {message}");
        }
        Ok(resp.json::<ResolveResponse>().await?.packs)
    }

    /// Is the registry actually there?
    ///
    /// `/health` is the one route that needs no credential, so this
    /// separates "the URL is wrong" from "the token is wrong" -- the two
    /// failures that otherwise arrive as the same warn line at dispatch.
    /// The error names the URL because the whole class of bug here is a URL
    /// that looks right from the outside and points nowhere from inside a
    /// container.
    pub async fn health(&self) -> anyhow::Result<()> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("{url} returned {status}");
        }
        Ok(())
    }
}

/// Minimal percent-encoding for the one query parameter this sends. Pack
/// names are kebab-case and ranges are semver, so only a few characters
/// can appear that need escaping.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_lists_survive_encoding() {
        assert_eq!(urlencode("office,essentials"), "office,essentials");
        // `@` and `*` appear in ranges and must not break the query string.
        assert_eq!(urlencode("bgp@1.2"), "bgp%401.2");
        assert_eq!(urlencode("a@>=1,b"), "a%40%3E%3D1,b");
    }

    #[test]
    fn the_base_url_is_normalised() {
        let c = SkillsClient::new("https://skills.puku.sh/".into(), None);
        assert_eq!(c.base_url, "https://skills.puku.sh");
    }

    /// The point of the probe is to name the address that failed, because
    /// the address is the bug. Port 1 refuses immediately, so this stays
    /// offline and fast.
    #[tokio::test]
    async fn health_names_the_url_it_could_not_reach() {
        let c = SkillsClient::new("http://127.0.0.1:1".into(), None);
        let err = c.health().await.unwrap_err().to_string();
        assert!(err.contains("http://127.0.0.1:1/health"), "{err}");
    }

    /// Without a caller bearer and without a service token there is no way
    /// to ask the registry anything — say so rather than sending an
    /// unauthenticated request that will 401 confusingly.
    #[tokio::test]
    async fn resolving_with_no_credential_at_all_is_an_error() {
        let c = SkillsClient::new("http://127.0.0.1:1".into(), None);
        let err = c.resolve(None, uuid::Uuid::nil(), &["office".into()]).await.unwrap_err();
        assert!(err.to_string().contains("PUKU_SKILLS_TOKEN"), "{err}");
    }
}
