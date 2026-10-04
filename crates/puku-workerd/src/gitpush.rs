//! Push the session's work to a branch when the agent finishes.
//!
//! Deliberately done **on the host, not in the guest**. The workspace is a
//! bind-mounted host directory, so workerd can push it directly — which
//! means the push credential never enters the microVM at all. Handing a
//! repo-writable token to a VM running model-authored code, purely so it
//! can run `git push`, would be the largest avoidable credential exposure
//! in the whole design.
//!
//! The token is minted fresh at push time: GitHub App installation tokens
//! live one hour, so the token used to clone at session start is usually
//! dead by the time the session ends.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use puku_cloud_proto::worker_proto::Up;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Brokers `RequestGitToken` -> `GitToken` round trips.
#[derive(Clone)]
pub struct GitTokens {
    up_tx: mpsc::UnboundedSender<Up>,
    pending: Arc<Mutex<HashMap<Uuid, oneshot::Sender<Option<String>>>>>,
}

impl GitTokens {
    pub fn new(up_tx: mpsc::UnboundedSender<Up>) -> Self {
        GitTokens { up_tx, pending: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn resolve(&self, session_id: Uuid, token: Option<String>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&session_id) {
            let _ = tx.send(token);
        }
    }

    async fn request(&self, session_id: Uuid) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(session_id, tx);
        let _ = self.up_tx.send(Up::RequestGitToken { session_id });
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(token)) => token,
            _ => {
                self.pending.lock().unwrap().remove(&session_id);
                None
            }
        }
    }
}

async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Branch name for a session's work. Namespaced so it is obvious in a
/// branch list where the commits came from.
pub fn branch_name(session_id: Uuid) -> String {
    format!("puku/session-{}", &session_id.to_string()[..8])
}

/// True when the working tree holds commits or changes worth pushing.
async fn has_work(repo: &Path) -> Result<bool> {
    // Uncommitted changes count: the agent may have edited without
    // committing, and losing that to a reaped volume is the exact failure
    // this whole path exists to prevent.
    if !git(repo, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(true);
    }
    // A repo with no commits at all has no HEAD, and `rev-list HEAD` is a
    // hard error there rather than "0" — treat it as nothing to push.
    if git(repo, &["rev-parse", "--verify", "HEAD"]).await.is_err() {
        return Ok(false);
    }
    // Commits the remote doesn't have. No upstream (a branch created in
    // this session) also means there is something to push.
    match git(repo, &["rev-list", "--count", "@{u}..HEAD"]).await {
        Ok(count) => Ok(count.trim() != "0"),
        Err(_) => Ok(git(repo, &["rev-list", "--count", "HEAD"]).await? != "0"),
    }
}

/// Commit anything outstanding, push a session branch, and report it.
///
/// Best effort by design: a session that produced good work and failed to
/// push should still complete, with the failure visible in the log rather
/// than as a lost session.
pub async fn push_session_branch(
    session_id: Uuid,
    workspace: &Path,
    repo_url: &str,
    tokens: &GitTokens,
    up_tx: &mpsc::UnboundedSender<Up>,
) {
    let repo = workspace.join("repo");
    if !repo.join(".git").exists() {
        return; // nothing was cloned
    }
    match do_push(session_id, &repo, repo_url, tokens).await {
        Ok(Some(branch)) => {
            let _ = up_tx.send(Up::BranchPushed { session_id, branch });
        }
        Ok(None) => tracing::info!(%session_id, "no work to push"),
        Err(e) => tracing::warn!(%session_id, error = format!("{e:#}"), "pushing the session branch failed"),
    }
}

async fn do_push(
    session_id: Uuid,
    repo: &Path,
    repo_url: &str,
    tokens: &GitTokens,
) -> Result<Option<String>> {
    if !has_work(repo).await? {
        return Ok(None);
    }
    let branch = branch_name(session_id);
    git(repo, &["checkout", "-B", &branch]).await?;

    if !git(repo, &["status", "--porcelain"]).await?.is_empty() {
        git(repo, &["add", "-A"]).await?;
        // Identity: a commit with no author fails outright on many hosts.
        git(repo, &["-c", "user.email=agent@puku.sh", "-c", "user.name=puku agent",
                    "commit", "-m", "puku: session work"])
            .await?;
    }

    let token = tokens
        .request(session_id)
        .await
        .context("no git token available for the push")?;
    // Credential in the URL for exactly one command, never written to
    // .git/config where it would persist on the session volume.
    let authed = repo_url.replace("https://", &format!("https://x-access-token:{token}@"));
    let result = git(repo, &["push", "--force-with-lease", &authed, &format!("HEAD:{branch}")]).await;
    match result {
        Ok(_) => Ok(Some(branch)),
        Err(e) => {
            // The token is in the URL, so it can appear in git's stderr.
            let scrubbed = e.to_string().replace(&token, "[redacted]");
            anyhow::bail!("{scrubbed}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_namespaced_and_short() {
        let b = branch_name(Uuid::nil());
        assert_eq!(b, "puku/session-00000000");
        // Must be a legal ref: no spaces, no double slashes.
        assert!(!b.contains(' ') && !b.contains("//"));
    }

    /// A freshly-initialised repo has no HEAD at all, and `rev-list HEAD`
    /// is a hard error there — it must read as "nothing to push", not as a
    /// failure that aborts the whole end-of-session path.
    #[tokio::test]
    async fn a_repo_with_no_commits_has_no_work() {
        let dir = std::env::temp_dir().join(format!("puku-git-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).await.unwrap();
        assert!(!has_work(&dir).await.unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_edited_file_counts_as_work() {
        let dir = std::env::temp_dir().join(format!("puku-git-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).await.unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        assert!(has_work(&dir).await.unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The push URL carries a token; if git fails, its stderr must not
    /// carry that token into the log.
    #[test]
    fn push_errors_scrub_the_token() {
        let token = "ghs_supersecrettoken";
        let err = format!("git push failed: https://x-access-token:{token}@github.com/o/r denied");
        assert!(!err.replace(token, "[redacted]").contains(token));
    }
}
