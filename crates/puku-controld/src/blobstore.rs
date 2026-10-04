//! Object storage for anything too big for a Postgres row: spilled event
//! blobs, workspace/transcript tarballs, archived event logs.
//!
//! **Credentials live here, on controld, and nowhere else.** Workers ask
//! for a presigned URL over the existing worker WebSocket and PUT straight
//! to R2. A worker is the less trusted tier — it runs model-authored code
//! one boundary away and, until per-worker tokens landed, any host with the
//! shared secret could join the fleet — so handing every worker a bucket
//! key would make the blast radius of one compromised box the whole store.
//!
//! S3-compatible: Cloudflare R2 in production, MinIO or localstack in dev.

use std::time::Duration;

use anyhow::{Context, Result};
use rusty_s3::actions::CreateMultipartUpload;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use uuid::Uuid;

/// How long a presigned URL stays valid. Long enough for a slow upload of a
/// large tarball on a bad link, short enough that a leaked URL is a small
/// window.
const PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct BlobStore {
    bucket: Bucket,
    credentials: Credentials,
    /// Recorded in `blob_ref` so a stored reference stays resolvable after
    /// an endpoint change.
    bucket_name: String,
}

impl std::fmt::Debug for BlobStore {
    // Never let the secret key reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStore").field("bucket", &self.bucket_name).finish()
    }
}

impl BlobStore {
    /// Build from config. Returns `Ok(None)` when object storage isn't
    /// configured, so a dev box runs without it — the callers degrade to
    /// "no blob upload" rather than failing.
    pub fn from_env(
        endpoint: Option<&str>,
        bucket: Option<&str>,
        region: &str,
        access_key: Option<&str>,
        secret_key: Option<&str>,
    ) -> Result<Option<Self>> {
        let (Some(endpoint), Some(bucket_name), Some(access_key), Some(secret_key)) =
            (endpoint, bucket, access_key, secret_key)
        else {
            return Ok(None);
        };
        let url = endpoint.parse().context("PUKU_R2_ENDPOINT is not a URL")?;
        // Path style: R2's S3 endpoint is account-scoped
        // (https://<account>.r2.cloudflarestorage.com/<bucket>/<key>), and
        // MinIO defaults to path style too.
        let bucket = Bucket::new(url, UrlStyle::Path, bucket_name.to_string(), region.to_string())
            .context("building the S3 bucket handle")?;
        Ok(Some(BlobStore {
            bucket,
            credentials: Credentials::new(access_key, secret_key),
            bucket_name: bucket_name.to_string(),
        }))
    }

    /// Stable key for a spilled event line. Derived, not stored, so the
    /// worker can name the object before the upload finishes and the event
    /// can be persisted immediately.
    pub fn blob_key(session_id: Uuid, guest_line: i64) -> String {
        format!("sessions/{session_id}/blobs/line-{guest_line}.json")
    }

    /// Key for a packaged workspace / transcript tarball.
    pub fn artifact_key(session_id: Uuid, what: &str) -> String {
        format!("sessions/{session_id}/artifacts/{what}.tgz")
    }

    /// A transcript teleported up from a local session.
    pub fn import_key(session_id: Uuid) -> String {
        format!("sessions/{session_id}/import/transcript.jsonl")
    }

    pub fn archive_key(session_id: Uuid) -> String {
        format!("sessions/{session_id}/events.ndjson")
    }

    /// The durable reference stored in `session_events.blob_ref`. Carries
    /// the bucket so a reference survives a config change, unlike the old
    /// `session://blobs/…` form which pointed at a worker-local path that
    /// got reaped.
    pub fn blob_ref(&self, key: &str) -> String {
        format!("s3://{}/{}", self.bucket_name, key)
    }

    /// Extract the key from a `blob_ref` this store produced.
    pub fn key_from_ref(&self, blob_ref: &str) -> Option<String> {
        blob_ref
            .strip_prefix(&format!("s3://{}/", self.bucket_name))
            .map(|k| k.to_string())
    }

    pub fn presign_put(&self, key: &str) -> String {
        self.bucket
            .put_object(Some(&self.credentials), key)
            .sign(PRESIGN_TTL)
            .to_string()
    }

    pub fn presign_get(&self, key: &str) -> String {
        self.bucket
            .get_object(Some(&self.credentials), key)
            .sign(PRESIGN_TTL)
            .to_string()
    }

    /// Round-trip a tiny object to prove the credentials actually work.
    ///
    /// `/health` used to report object storage healthy whenever it was
    /// *configured*, which is the least useful moment to be optimistic: a
    /// wrong access key stays green here and first shows up much later as
    /// an unreadable deliverable, with the operator looking at a 403 from
    /// a presigned URL and no idea which end is wrong.
    pub async fn probe(&self) -> Result<()> {
        let key = Self::health_key();
        self.put(&key, b"ok".to_vec(), "text/plain")
            .await
            .context("object storage write probe failed (check the access key and bucket)")?;
        let resp = reqwest::Client::new()
            .get(self.presign_get(&key))
            .send()
            .await
            .context("object storage read probe failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("object storage read probe failed: {}", resp.status());
        }
        Ok(())
    }

    /// Fixed key, so the probe overwrites one object rather than littering
    /// the bucket with one per health check.
    pub fn health_key() -> String {
        "health/controld-probe".to_string()
    }

    // -- Multipart, for snapshots: a worker PUTs each part to a URL presigned
    // here, and only this side starts, completes, aborts or deletes.

    /// Start a multipart upload and return its id.
    pub async fn create_multipart(&self, key: &str) -> Result<String> {
        let url = self.bucket.create_multipart_upload(Some(&self.credentials), key).sign(PRESIGN_TTL);
        let resp = reqwest::Client::new().post(url).send().await.context("starting a multipart upload")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("starting a multipart upload failed: {status} {body}");
        }
        let parsed = CreateMultipartUpload::parse_response(&body)
            .map_err(|e| anyhow::anyhow!("reading the multipart upload id: {e}"))?;
        Ok(parsed.upload_id().to_string())
    }

    /// A URL for one part. Parts are numbered from 1.
    pub fn presign_upload_part(&self, key: &str, upload_id: &str, part: u16) -> String {
        self.bucket
            .upload_part(Some(&self.credentials), key, part, upload_id)
            .sign(PRESIGN_TTL)
            .to_string()
    }

    /// Stitch the parts into the object. `etags` in part order, exactly as
    /// the bucket returned them.
    pub async fn complete_multipart(&self, key: &str, upload_id: &str, etags: &[String]) -> Result<()> {
        let action =
            self.bucket
                .complete_multipart_upload(Some(&self.credentials), key, upload_id, etags.iter().map(String::as_str));
        let url = action.sign(PRESIGN_TTL);
        let body = action.body();
        let resp = reqwest::Client::new()
            .post(url)
            .body(body)
            .send()
            .await
            .context("completing a multipart upload")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // S3 can answer 200 and put the failure in the body, because it has
        // already started streaming by the time assembly fails.
        if !status.is_success() || text.contains("<Error>") {
            anyhow::bail!("completing a multipart upload failed: {status} {text}");
        }
        Ok(())
    }

    /// Give up on an upload, freeing whatever parts it has. An upload that
    /// is already gone is not an error.
    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        let url = self.bucket.abort_multipart_upload(Some(&self.credentials), key, upload_id).sign(PRESIGN_TTL);
        let resp = reqwest::Client::new().delete(url).send().await.context("aborting a multipart upload")?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("aborting a multipart upload failed: {}", resp.status());
        }
        Ok(())
    }

    /// Delete one object. A missing object is not an error.
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        let url = self.bucket.delete_object(Some(&self.credentials), key).sign(PRESIGN_TTL);
        let resp = reqwest::Client::new().delete(url).send().await.context("deleting an object")?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("deleting {key} failed: {}", resp.status());
        }
        Ok(())
    }

    /// Upload from controld itself (the archiver path). Workers never call
    /// this — they PUT to a presigned URL instead.
    pub async fn put(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        let url = self.presign_put(key);
        let resp = reqwest::Client::new()
            .put(url)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .context("uploading object")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("object upload failed: {status} {body}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> BlobStore {
        BlobStore::from_env(
            Some("https://acct.r2.cloudflarestorage.com"),
            Some("puku-cloud"),
            "auto",
            Some("AKIAEXAMPLE"),
            Some("secret"),
        )
        .unwrap()
        .unwrap()
    }

    /// Partial config must not half-configure the store; callers check for
    /// None and skip upload rather than erroring at runtime.
    #[test]
    fn missing_config_yields_no_store() {
        assert!(BlobStore::from_env(None, Some("b"), "auto", Some("k"), Some("s"))
            .unwrap()
            .is_none());
        assert!(BlobStore::from_env(Some("https://x.test"), None, "auto", Some("k"), Some("s"))
            .unwrap()
            .is_none());
        assert!(BlobStore::from_env(Some("https://x.test"), Some("b"), "auto", None, Some("s"))
            .unwrap()
            .is_none());
    }

    /// The key is derived from (session, line) alone so the worker can emit
    /// the event with its final reference before the PUT completes.
    #[test]
    fn blob_keys_are_deterministic() {
        let id = Uuid::nil();
        assert_eq!(
            BlobStore::blob_key(id, 42),
            "sessions/00000000-0000-0000-0000-000000000000/blobs/line-42.json"
        );
    }

    #[test]
    fn blob_ref_round_trips_to_a_key() {
        let s = store();
        let key = BlobStore::blob_key(Uuid::nil(), 7);
        let r = s.blob_ref(&key);
        assert!(r.starts_with("s3://puku-cloud/"), "{r}");
        assert_eq!(s.key_from_ref(&r).as_deref(), Some(key.as_str()));
    }

    /// A reference from a different bucket must not resolve against this
    /// one — that would serve the wrong object or 404 confusingly.
    #[test]
    fn foreign_refs_do_not_resolve() {
        let s = store();
        assert!(s.key_from_ref("s3://other-bucket/sessions/x/blobs/line-1.json").is_none());
        // The legacy worker-local form, which nothing ever uploaded.
        assert!(s.key_from_ref("session://blobs/line-1.json").is_none());
    }

    #[test]
    fn presigned_urls_are_signed_and_scoped_to_the_key() {
        let s = store();
        let key = BlobStore::blob_key(Uuid::nil(), 1);
        for url in [s.presign_put(&key), s.presign_get(&key)] {
            assert!(url.contains("X-Amz-Signature="), "{url}");
            assert!(url.contains("X-Amz-Expires="), "{url}");
            assert!(url.contains("line-1.json"), "{url}");
            // Path style keeps the bucket in the path, not the host.
            assert!(url.contains("/puku-cloud/sessions/"), "{url}");
            // The secret itself must never appear in a URL we hand out.
            assert!(!url.contains("secret"), "{url}");
        }
    }

    /// A part URL names its upload and part, or the bucket cannot tell which
    /// upload the bytes belong to.
    #[test]
    fn part_urls_carry_the_upload_and_part_number() {
        let url = store().presign_upload_part("machines/m/snapshots/s/volume.pks", "up-123", 7);
        assert!(url.contains("uploadId=up-123"), "{url}");
        assert!(url.contains("partNumber=7"), "{url}");
        assert!(url.contains("X-Amz-Signature="), "{url}");
    }
}
