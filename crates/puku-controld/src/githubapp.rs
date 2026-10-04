//! GitHub App integration: mint short-lived, repo-scoped installation
//! tokens for session git access. Configured via PUKU_GITHUB_APP_ID +
//! PUKU_GITHUB_APP_KEY_FILE (PEM private key). When unset, sessions fall
//! back to the static PUKU_GIT_TOKEN.

use anyhow::{Context, Result};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;

pub struct GithubApp {
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

impl GithubApp {
    pub fn from_env() -> Option<Self> {
        let app_id = std::env::var("PUKU_GITHUB_APP_ID").ok()?;
        let key_file = std::env::var("PUKU_GITHUB_APP_KEY_FILE").ok()?;
        let pem = match std::fs::read(&key_file) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, file = %key_file, "cannot read GitHub App key");
                return None;
            }
        };
        let key = match EncodingKey::from_rsa_pem(&pem) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "bad GitHub App private key");
                return None;
            }
        };
        tracing::info!(%app_id, "GitHub App configured for session git tokens");
        Some(GithubApp {
            app_id,
            key,
            http: reqwest::Client::builder()
                .user_agent("puku-agent-cloud")
                .build()
                .expect("http client"),
        })
    }

    fn app_jwt(&self) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let claims = Claims {
            iat: now.saturating_sub(60),
            exp: now + 540, // GitHub max is 10 minutes
            iss: self.app_id.clone(),
        };
        Ok(jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)?)
    }

    /// Mint an installation token scoped to a single repository, valid ≤1h.
    pub async fn installation_token(&self, repo_url: &str) -> Result<String> {
        let (owner, repo) = parse_github_repo(repo_url)
            .with_context(|| format!("not a github repo url: {repo_url}"))?;
        let jwt = self.app_jwt()?;

        let inst: serde_json::Value = self
            .http
            .get(format!("https://api.github.com/repos/{owner}/{repo}/installation"))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()
            .context("looking up app installation for repo")?
            .json()
            .await?;
        let inst_id = inst["id"].as_i64().context("installation id missing")?;

        let tok: serde_json::Value = self
            .http
            .post(format!("https://api.github.com/app/installations/{inst_id}/access_tokens"))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({"repositories": [repo]}))
            .send()
            .await?
            .error_for_status()
            .context("minting installation token")?
            .json()
            .await?;
        tok["token"]
            .as_str()
            .map(str::to_string)
            .context("token missing in response")
    }

/// Open a pull request for a pushed branch. Returns its URL.
///
/// Sessions used to end in a transcript and a reaped volume; the whole
/// point of pushing a branch is that a human can review it where they
/// review everything else.
pub async fn open_pull_request(
    &self,
    repo_url: &str,
    head: &str,
    base: Option<&str>,
    title: &str,
    body: &str,
) -> Result<String> {
    let (owner, repo) = parse_github_repo(repo_url)
        .with_context(|| format!("not a github repo url: {repo_url}"))?;
    let token = self.installation_token(repo_url).await?;

    // Default to the repo's own default branch rather than assuming "main":
    // plenty of repos are still on master, or on a release branch.
    let base = match base {
        Some(b) => b.to_string(),
        None => {
            let meta: serde_json::Value = self
                .http
                .get(format!("https://api.github.com/repos/{owner}/{repo}"))
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await?
                .error_for_status()
                .context("reading repo metadata")?
                .json()
                .await?;
            meta["default_branch"].as_str().unwrap_or("main").to_string()
        }
    };

    let resp = self
        .http
        .post(format!("https://api.github.com/repos/{owner}/{repo}/pulls"))
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .json(&serde_json::json!({
            "title": title, "head": head, "base": base, "body": body,
        }))
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
        // GitHub uses 422 both for "a PR already exists for this branch"
        // and for real validation errors. An existing PR is success as far
        // as the session is concerned — find and return it.
        if let Ok(existing) = self.find_pull_request(&owner, &repo, head, &token).await {
            return Ok(existing);
        }
    }
    let pr: serde_json::Value = resp
        .error_for_status()
        .context("opening pull request")?
        .json()
        .await?;
    pr["html_url"]
        .as_str()
        .map(str::to_string)
        .context("pull request url missing in response")
}

async fn find_pull_request(
    &self,
    owner: &str,
    repo: &str,
    head: &str,
    token: &str,
) -> Result<String> {
    let list: serde_json::Value = self
        .http
        .get(format!("https://api.github.com/repos/{owner}/{repo}/pulls"))
        .query(&[("head", format!("{owner}:{head}")), ("state", "open".into())])
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    list.get(0)
        .and_then(|pr| pr["html_url"].as_str())
        .map(str::to_string)
        .context("no open pull request for that branch")
}
}

fn parse_github_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner, repo))
}
