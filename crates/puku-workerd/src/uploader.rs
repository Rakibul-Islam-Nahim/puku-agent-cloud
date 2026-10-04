//! Ships oversized event payloads off the worker and into object storage.
//!
//! The runner caps every outbox line and spills the full payload to
//! `/session/blobs/line-N.json`. Before this module existed the platform
//! recorded `blob_ref = "session://blobs/…"` and **nothing ever uploaded
//! the file**: the reference pointed at a worker-local path on a volume
//! that gets reaped, so every truncated event — which is to say every large
//! tool output — was permanently unrecoverable behind a dangling pointer.
//!
//! The worker holds no bucket credentials (see controld's `blobstore`), so
//! each upload is a round trip: ask controld for a presigned PUT, then PUT
//! the bytes. Uploads run off the tail loop; the event is emitted
//! immediately with its final, deterministic reference, because the key is
//! derived from (session, line) and does not depend on the PUT completing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use puku_cloud_proto::worker_proto::Up;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// A file waiting for its presigned URL.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Option<String>>>>>;

#[derive(Clone)]
pub struct Uploader {
    up_tx: mpsc::UnboundedSender<Up>,
    pending: Pending,
}

/// Same shape as the controld side; kept in sync by
/// `blob_key_matches_controld` in the tests there and here.
pub fn blob_key(session_id: Uuid, guest_line: i64) -> String {
    format!("sessions/{session_id}/blobs/line-{guest_line}.json")
}

impl Uploader {
    pub fn new(up_tx: mpsc::UnboundedSender<Up>) -> Self {
        Uploader { up_tx, pending: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Resolve a `Down::UploadUrl`. Unknown keys are dropped: a duplicate or
    /// late answer must not panic the link.
    pub fn resolve(&self, key: &str, url: Option<String>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(key) {
            let _ = tx.send(url);
        }
    }

    /// Upload `path` in the background. Returns immediately.
    pub fn upload(&self, session_id: Uuid, key: String, path: PathBuf, content_type: &str) {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), tx);
        let _ = self.up_tx.send(Up::RequestUpload {
            session_id,
            key: key.clone(),
            content_type: content_type.to_string(),
        });
        let pending = self.pending.clone();
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            // Bounded wait: a controld that never answers must not leak the
            // entry, and the reconnect path would re-request anyway.
            let url = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                Ok(Ok(Some(url))) => url,
                Ok(Ok(None)) => {
                    tracing::debug!(%key, "no object storage configured; blob not uploaded");
                    return;
                }
                Ok(Err(_)) | Err(_) => {
                    pending.lock().unwrap().remove(&key);
                    tracing::warn!(%key, "no upload url from controld; blob not uploaded");
                    return;
                }
            };
            match tokio::fs::read(&path).await {
                Ok(body) => {
                    let len = body.len();
                    match reqwest::Client::new()
                        .put(url)
                        .header("content-type", content_type)
                        .body(body)
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            tracing::info!(%key, bytes = len, "blob uploaded");
                        }
                        Ok(r) => tracing::warn!(%key, status = %r.status(), "blob upload rejected"),
                        Err(e) => tracing::warn!(%key, error = %e, "blob upload failed"),
                    }
                }
                Err(e) => tracing::warn!(%key, path = %path.display(), error = %e, "blob unreadable"),
            }
        });
    }
}

/// Paths never to put in an artifact, relative to the tar root.
///
/// `/session/home` is the guest's `$HOME`, so packaging it wholesale shipped
/// `.config/pukucode/session.json` -- puku-cli's own credential file, holding a
/// live access token *and* a refresh token. Anyone who could call
/// `GET /v1/sessions/{id}/artifacts/home` got the session owner's login.
///
/// Excluded here rather than only client-side because the tarball is what
/// lands in object storage: a client-side skip protects the person who
/// downloads it, not the object sitting in a bucket.
const ARTIFACT_EXCLUDES: &[&str] = &["./.config/pukucode/session.json"];

/// Tar+gzip a directory into `dest`. Used to package `/workspace` or
/// `/session/home` out of a live session.
///
/// Shells out to tar rather than pulling in a tar+flate crate: the host
/// already has it, and streaming a multi-gigabyte workspace through the
/// process heap would be worse than spawning a process.
pub async fn tar_directory(src: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    if !src.exists() {
        anyhow::bail!("{} does not exist", src.display());
    }
    let mut cmd = tokio::process::Command::new("tar");
    // -C so the archive holds relative paths, not the worker's layout.
    cmd.arg("-czf").arg(dest).arg("-C").arg(src);
    for pattern in ARTIFACT_EXCLUDES {
        // Before `.`: BSD tar only honours --exclude when it precedes the
        // paths it applies to, and the worker fleet is Linux while the tests
        // run on macOS. GNU tar accepts either order.
        cmd.arg(format!("--exclude={pattern}"));
    }
    let status = cmd
        .arg(".")
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("tar exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker names the object; controld presigns exactly that name.
    /// If these two ever drift, every upload 403s on a signature mismatch.
    #[test]
    fn blob_key_matches_the_controld_layout() {
        assert_eq!(
            blob_key(Uuid::nil(), 42),
            "sessions/00000000-0000-0000-0000-000000000000/blobs/line-42.json"
        );
    }

    #[tokio::test]
    async fn requests_a_url_for_the_key_it_will_upload() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let up = Uploader::new(tx);
        let key = blob_key(Uuid::nil(), 1);
        up.upload(Uuid::nil(), key.clone(), PathBuf::from("/nonexistent"), "application/json");
        match rx.recv().await.expect("a frame") {
            Up::RequestUpload { key: k, content_type, .. } => {
                assert_eq!(k, key);
                assert_eq!(content_type, "application/json");
            }
            other => panic!("expected RequestUpload, got {other:?}"),
        }
    }

    /// A late or duplicate answer for a key nobody is waiting on must be
    /// ignored rather than panicking the control link.
    #[tokio::test]
    async fn resolving_an_unknown_key_is_harmless() {
        let (tx, _rx) = mpsc::unbounded_channel();
        Uploader::new(tx).resolve("sessions/x/blobs/line-9.json", Some("http://x".into()));
    }

    #[tokio::test]
    async fn tars_a_directory_with_relative_paths() {
        let dir = std::env::temp_dir().join(format!("puku-tar-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main() {}").unwrap();
        let dest = dir.with_extension("tgz");
        tar_directory(&dir, &dest).await.unwrap();

        // Relative entries: unpacking must not recreate the worker's layout.
        let out = std::process::Command::new("tar").arg("-tzf").arg(&dest).output().unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(listing.contains("./src/main.rs"), "{listing}");
        assert!(!listing.contains("/var/lib/puku"), "{listing}");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&dest).ok();
    }

    /// The home artifact is the guest's whole $HOME, and puku-cli keeps its
    /// login there. Shipping that to anyone who can download the artifact is
    /// a credential leak, so the tar must drop it.
    #[tokio::test]
    async fn an_artifact_never_carries_the_guest_credential_file() {
        let dir = std::env::temp_dir().join(format!("puku-home-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".config/pukucode")).unwrap();
        std::fs::write(
            dir.join(".config/pukucode/session.json"),
            br#"{"accessToken":"SHOULD-NOT-SHIP","refreshToken":"NOR-THIS"}"#,
        )
        .unwrap();
        // Something legitimate alongside it, so a test that simply tars
        // nothing cannot pass.
        std::fs::create_dir_all(dir.join(".puku-cli/projects/-workspace")).unwrap();
        std::fs::write(dir.join(".puku-cli/projects/-workspace/abc.jsonl"), b"{}\n").unwrap();

        let dest = dir.with_extension("tgz");
        tar_directory(&dir, &dest).await.unwrap();

        let out = std::process::Command::new("tar").arg("-tzf").arg(&dest).output().unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listing.contains("session.json"),
            "the guest credential file reached the artifact: {listing}"
        );
        assert!(
            listing.contains("abc.jsonl"),
            "the transcript must still be there, or teleport-down has nothing to read: {listing}"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&dest).ok();
    }

    /// A session whose workspace never materialized must produce a clear
    /// error, not an empty tarball the user then can't explain.
    #[tokio::test]
    async fn a_missing_directory_is_an_error() {
        let err = tar_directory(
            std::path::Path::new("/nonexistent/puku/workspace"),
            std::path::Path::new("/tmp/should-not-exist.tgz"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }
}
