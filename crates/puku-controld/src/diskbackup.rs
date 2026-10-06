//! Off-cluster backup of shared disks (docs/RELIABILITY-REBUILD.md §4.7).
//!
//! Ceph keeps three copies of every disk, which covers a lost drive or a
//! lost host -- not a lost pool (a bad upgrade, an operator mistake, the
//! cluster itself). This keeps an independent copy in object storage.
//!
//! **Backup**, per session or machine on a shared disk, every
//! `interval`:
//! 1. RBD snapshot `bk-<ms>-<backup id>` of the image (crash-consistent; no pause).
//! 2. First time, or after `DIFF_CAP` diffs: `rbd export` the whole image.
//!    Otherwise `rbd export-diff` from the previous backup's snapshot -- and
//!    skip entirely when nothing changed (an idle disk costs nothing).
//! 3. Compress and seal it in 4 MiB frames with a fresh data key (the key is
//!    stored sealed under `PUKU_SECRET_KEY`), upload it as a multipart
//!    object, then **read it back** and check every frame and the sha256.
//!    Only then is the row `Durable`.
//! 4. Older backup snapshots are dropped from the image; a new full retires
//!    the previous chain (objects and rows, newest first).
//!
//! **Restore**: when a shared disk is about to be used and its image is
//! gone, the latest full and every later diff are imported, in order, into
//! a fresh image -- before any worker can create an empty one in its place.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::blobstore::BlobStore;
use crate::secretbox::SecretBox;
use crate::storagegc::PoolAdmin;
use crate::AppState;

/// Diffs after a full before the next backup is a full again.
pub const DIFF_CAP: usize = 24;
/// Plaintext per frame.
const FRAME: usize = 4 << 20;
/// Sealed bytes per uploaded part (S3 needs >= 5 MiB except the last).
const PART: usize = 8 << 20;
const MAGIC: &[u8] = b"PKBK1\n";

#[derive(Debug, Clone)]
pub struct BackupPolicy {
    pub interval: Duration,
    pub scan: Duration,
    pub tmp_dir: PathBuf,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            scan: Duration::from_secs(300),
            tmp_dir: std::env::temp_dir().join("puku-disk-backups"),
        }
    }
}

/// Whose disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Session(Uuid),
    Machine(Uuid),
}

impl Subject {
    pub fn image(&self) -> String {
        match self {
            Self::Session(id) => id.to_string(),
            Self::Machine(id) => format!("machine-{id}"),
        }
    }
    fn column(&self) -> &'static str {
        match self {
            Self::Session(_) => "session_id",
            Self::Machine(_) => "machine_id",
        }
    }
    fn id(&self) -> Uuid {
        match self {
            Self::Session(id) | Self::Machine(id) => *id,
        }
    }
    fn table(&self) -> &'static str {
        match self {
            Self::Session(_) => "sessions",
            Self::Machine(_) => "machines",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupResult {
    /// No image (never booted on a shared disk), or nothing changed.
    Skipped(&'static str),
    Full(Uuid),
    Diff(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreResult {
    Present,
    Restored { full: Uuid, diffs: usize },
    NoBackup,
}

/// One row of the subject's current chain.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: Uuid,
    kind: String,
    to_snap: String,
    r2_key: String,
    sha256: String,
    dek_sealed: Option<Vec<u8>>,
}

/// Everything a backup or restore needs.
pub struct Backups<'a> {
    pub pool: &'a sqlx::PgPool,
    pub ops: Arc<dyn PoolAdmin>,
    pub blobs: Arc<BlobStore>,
    pub secrets: Arc<SecretBox>,
    pub tmp_dir: PathBuf,
}

impl<'a> Backups<'a> {
    pub fn from_state(state: &'a AppState, tmp_dir: PathBuf) -> Option<Self> {
        Some(Self {
            pool: &state.pool,
            ops: state.shared_volumes.as_ref()?.admin.clone(),
            blobs: state.blobs.clone()?,
            secrets: state.secrets.clone()?,
            tmp_dir,
        })
    }

    /// The Durable rows from the latest full on, oldest first.
    async fn chain(&self, subject: Subject) -> Result<Vec<Row>> {
        let col = subject.column();
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT id, kind, to_snap, r2_key, sha256, dek_sealed FROM disk_backups              WHERE {col} = $1 AND status = 'Durable' ORDER BY ts, id"
        ))
        .bind(subject.id())
        .fetch_all(self.pool)
        .await?;
        Ok(match rows.iter().rposition(|r| r.kind == "full") {
            None => Vec::new(),
            Some(i) => rows[i..].to_vec(),
        })
    }

    pub async fn backup_once(&self, subject: Subject) -> Result<BackupResult> {
        let image = subject.image();
        if !self.ops.exists(&image).await? {
            return Ok(BackupResult::Skipped("no disk"));
        }
        let chain = self.chain(subject).await?;
        let diffs = chain.iter().filter(|r| r.kind == "diff").count();
        let from = (!chain.is_empty() && diffs < DIFF_CAP).then(|| chain.last().unwrap().to_snap.clone());
        let id = Uuid::new_v4();
        // The backup's own id in the name: two backups in one millisecond
        // would otherwise ask Ceph for the same snapshot twice.
        let snap = format!("bk-{}-{}", chrono::Utc::now().timestamp_millis(), &id.simple().to_string()[..8]);
        self.ops.snap_create(&image, &snap).await?;
        if let Some(from) = &from {
            if !self.ops.changed_since(&image, from, &snap).await? {
                self.ops.snap_remove(&image, &snap).await?;
                self.touch(subject).await?;
                return Ok(BackupResult::Skipped("nothing changed"));
            }
        }

        std::fs::create_dir_all(&self.tmp_dir)?;
        let plain = self.tmp_dir.join(format!("{id}.export"));
        let plain_s = plain.to_string_lossy().to_string();
        let exported = match &from {
            Some(from) => self.ops.export_diff(&image, from, &snap, &plain_s).await,
            None => self.ops.export_full(&image, &snap, &plain_s).await,
        };
        if let Err(e) = exported {
            let _ = std::fs::remove_file(&plain);
            let _ = self.ops.snap_remove(&image, &snap).await;
            return Err(e.context("exporting the disk"));
        }
        let kind = if from.is_some() { "diff" } else { "full" };
        let key = format!("backups/{}/{id}.{kind}", image);
        let result = self.store(id, &key, &plain).await;
        let _ = std::fs::remove_file(&plain);
        let (size, sha, dek_sealed) = match result {
            Ok(r) => r,
            Err(e) => {
                let _ = self.ops.snap_remove(&image, &snap).await;
                return Err(e);
            }
        };

        sqlx::query(&format!(
            "INSERT INTO disk_backups (id, {col}, kind, from_snap, to_snap, r2_key, size_bytes, sha256, status, dek_sealed) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'Pending', $9)",
            col = subject.column()
        ))
        .bind(id)
        .bind(subject.id())
        .bind(kind)
        .bind(&from)
        .bind(&snap)
        .bind(&key)
        .bind(size as i64)
        .bind(&sha)
        .bind(&dek_sealed)
        .execute(self.pool)
        .await?;

        // Durable only once what is in the bucket reads back right.
        let verified = self.verify(id, &key, &sha, &dek_sealed).await;
        let status = if verified.is_ok() { "Durable" } else { "Corrupt" };
        sqlx::query("UPDATE disk_backups SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(status)
            .execute(self.pool)
            .await?;
        if let Err(e) = verified {
            let _ = self.ops.snap_remove(&image, &snap).await;
            return Err(e.context("the uploaded backup did not read back intact"));
        }
        self.touch(subject).await?;

        // The image keeps only the snapshot the next diff starts from.
        for old in self.ops.snap_list(&image).await? {
            if old.starts_with("bk-") && old != snap {
                self.ops.snap_remove(&image, &old).await?;
            }
        }
        if kind == "full" {
            self.retire_before(subject, id).await?;
        }
        tracing::info!(subject = ?subject, backup = %id, kind, bytes = size, "disk backed up");
        Ok(if kind == "full" { BackupResult::Full(id) } else { BackupResult::Diff(id) })
    }

    async fn touch(&self, subject: Subject) -> Result<()> {
        sqlx::query(&format!("UPDATE {} SET last_disk_backup_at = now() WHERE id = $1", subject.table()))
            .bind(subject.id())
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Drop every backup of `subject` older than the full `keep`, newest
    /// first (children before parents).
    async fn retire_before(&self, subject: Subject, keep: Uuid) -> Result<()> {
        let col = subject.column();
        let old: Vec<(Uuid, String)> = sqlx::query_as(&format!(
            "SELECT id, r2_key FROM disk_backups WHERE {col} = $1 AND id <> $2 \
               AND ts <= (SELECT ts FROM disk_backups WHERE id = $2) ORDER BY ts DESC"
        ))
        .bind(subject.id())
        .bind(keep)
        .fetch_all(self.pool)
        .await?;
        for (id, key) in old {
            self.blobs.delete_object(&key).await?;
            sqlx::query("DELETE FROM disk_backups WHERE id = $1").bind(id).execute(self.pool).await?;
        }
        Ok(())
    }

    /// Seal `plain` into the bucket at `key`. Returns (stored bytes,
    /// plaintext sha256, sealed data key).
    async fn store(&self, id: Uuid, key: &str, plain: &Path) -> Result<(u64, String, Vec<u8>)> {
        let mut dek = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut dek);
        let dek_sealed = self.secrets.encrypt(&hex::encode(dek))?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&dek));

        let upload = self.blobs.create_multipart(key).await?;
        let outcome = async {
            let mut file = tokio::fs::File::open(plain).await?;
            let mut sha = Sha256::new();
            let mut part = MAGIC.to_vec();
            let mut etags = Vec::new();
            let mut stored = 0u64;
            let mut index = 0u64;
            let mut buf = vec![0u8; FRAME];
            loop {
                let n = read_full(&mut file, &mut buf).await?;
                let last = n < FRAME;
                sha.update(&buf[..n]);
                let sealed = seal(&cipher, id, index, last, &buf[..n])?;
                part.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
                part.extend_from_slice(&sealed);
                index += 1;
                if part.len() >= PART || last {
                    stored += part.len() as u64;
                    let n_part = u16::try_from(etags.len() + 1).context("backup too large for one upload")?;
                    etags.push(self.put_part(key, &upload, n_part, std::mem::take(&mut part)).await?);
                }
                if last {
                    break;
                }
            }
            self.blobs.complete_multipart(key, &upload, &etags).await?;
            Ok::<_, anyhow::Error>((stored, hex::encode(sha.finalize())))
        }
        .await;
        match outcome {
            Ok((stored, sha)) => Ok((stored, sha, dek_sealed)),
            Err(e) => {
                let _ = self.blobs.abort_multipart(key, &upload).await;
                Err(e)
            }
        }
    }

    async fn put_part(&self, key: &str, upload: &str, n: u16, body: Vec<u8>) -> Result<String> {
        let url = self.blobs.presign_upload_part(key, upload, n);
        let resp = reqwest::Client::new().put(url).body(body).send().await.context("uploading a backup part")?;
        if !resp.status().is_success() {
            bail!("uploading backup part {n} failed: {}", resp.status());
        }
        Ok(resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .context("the bucket returned no ETag")?
            .to_string())
    }

    /// Read `key` back and check it opens and hashes to `sha`.
    async fn verify(&self, id: Uuid, key: &str, sha: &str, dek_sealed: &[u8]) -> Result<()> {
        let got = self.fetch(id, key, dek_sealed, None).await?;
        if got != sha {
            bail!("sha256 mismatch: stored {got}, expected {sha}");
        }
        Ok(())
    }

    /// Stream `key`, open every frame, and return the plaintext sha256;
    /// with `out`, also write the plaintext there.
    async fn fetch(&self, id: Uuid, key: &str, dek_sealed: &[u8], out: Option<&Path>) -> Result<String> {
        let dek = hex::decode(self.secrets.decrypt(dek_sealed)?)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&dek));
        let mut resp = reqwest::get(self.blobs.presign_get(key)).await.context("downloading a backup")?;
        if !resp.status().is_success() {
            bail!("downloading {key} failed: {}", resp.status());
        }
        let mut file = match out {
            Some(p) => Some(tokio::fs::File::create(p).await?),
            None => None,
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut sha = Sha256::new();
        let mut index = 0u64;
        let mut magic_seen = false;
        let mut done = false;
        loop {
            // Drain whole frames from what has arrived.
            loop {
                if !magic_seen {
                    if pending.len() < MAGIC.len() {
                        break;
                    }
                    if &pending[..MAGIC.len()] != MAGIC {
                        bail!("not a puku disk backup");
                    }
                    pending.drain(..MAGIC.len());
                    magic_seen = true;
                }
                if done || pending.len() < 4 {
                    break;
                }
                let len = u32::from_be_bytes(pending[..4].try_into().unwrap()) as usize;
                if pending.len() < 4 + len {
                    break;
                }
                let sealed: Vec<u8> = pending.drain(..4 + len).skip(4).collect();
                let (plain, last) = open(&cipher, id, index, &sealed)?;
                sha.update(&plain);
                if let Some(f) = file.as_mut() {
                    f.write_all(&plain).await?;
                }
                index += 1;
                done = last;
            }
            match resp.chunk().await.context("reading a backup")? {
                Some(chunk) => pending.extend_from_slice(&chunk),
                None => break,
            }
        }
        if !done {
            bail!("the backup is truncated (no final frame)");
        }
        if !pending.is_empty() {
            bail!("trailing bytes after the final frame");
        }
        if let Some(mut f) = file {
            f.flush().await?;
        }
        Ok(hex::encode(sha.finalize()))
    }

    /// Make sure the subject's disk exists, rebuilding it from its backups
    /// when the image is gone.
    pub async fn restore_if_missing(&self, subject: Subject) -> Result<RestoreResult> {
        let image = subject.image();
        if self.ops.exists(&image).await? {
            return Ok(RestoreResult::Present);
        }
        let chain = self.chain(subject).await?;
        let Some(full) = chain.first() else { return Ok(RestoreResult::NoBackup) };
        std::fs::create_dir_all(&self.tmp_dir)?;
        for (i, row) in chain.iter().enumerate() {
            let path = self.tmp_dir.join(format!("{}.restore", row.id));
            let dek = row.dek_sealed.as_deref().context("backup row has no data key")?;
            let fetched = self.fetch(row.id, &row.r2_key, dek, Some(&path)).await;
            let applied = match fetched {
                Ok(sha) if sha != row.sha256 => Err(anyhow::anyhow!("backup {} fails its sha256", row.id)),
                Ok(_) => {
                    let p = path.to_string_lossy().to_string();
                    if i == 0 {
                        self.ops.import_full(&p, &image, &row.to_snap).await
                    } else {
                        self.ops.import_diff(&p, &image).await
                    }
                }
                Err(e) => Err(e),
            };
            let _ = std::fs::remove_file(&path);
            applied.with_context(|| format!("restoring backup {} of {image}", row.id))?;
        }
        crate::db::audit(self.pool, None, None, "disk.restored", &image, serde_json::json!({
            "full": full.id, "diffs": chain.len() - 1,
        }))
        .await
        .ok();
        tracing::warn!(%image, diffs = chain.len() - 1, "disk was missing; restored from its backups");
        Ok(RestoreResult::Restored { full: full.id, diffs: chain.len() - 1 })
    }
}

async fn read_full(file: &mut tokio::fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let got = file.read(&mut buf[n..]).await?;
        if got == 0 {
            break;
        }
        n += got;
    }
    Ok(n)
}

fn aad(id: Uuid, index: u64, last: bool) -> Vec<u8> {
    let mut a = id.as_bytes().to_vec();
    a.extend_from_slice(&index.to_be_bytes());
    a.push(last as u8);
    a
}

fn nonce(index: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&index.to_be_bytes());
    n
}

/// One frame: `[last][zstd(plain)]`, sealed. The frame index and the last
/// flag are bound in, so frames cannot be reordered, dropped or cut short.
fn seal(cipher: &ChaCha20Poly1305, id: Uuid, index: u64, last: bool, plain: &[u8]) -> Result<Vec<u8>> {
    let mut body = vec![last as u8];
    body.extend(zstd::encode_all(plain, 3)?);
    cipher
        .encrypt(Nonce::from_slice(&nonce(index)), Payload { msg: &body, aad: &aad(id, index, last) })
        .map_err(|_| anyhow::anyhow!("sealing a backup frame failed"))
}

fn open(cipher: &ChaCha20Poly1305, id: Uuid, index: u64, sealed: &[u8]) -> Result<(Vec<u8>, bool)> {
    // The flag is inside the ciphertext but bound in the AAD too: try both.
    for last in [false, true] {
        if let Ok(body) =
            cipher.decrypt(Nonce::from_slice(&nonce(index)), Payload { msg: sealed, aad: &aad(id, index, last) })
        {
            anyhow::ensure!(body.first() == Some(&(last as u8)), "frame flag mismatch");
            return Ok((zstd::decode_all(&body[1..])?, last));
        }
    }
    bail!("backup frame {index} does not open: wrong key, or the object was altered")
}

/// What may need a backup now.
async fn due(pool: &sqlx::PgPool, interval: Duration) -> Result<Vec<Subject>> {
    let secs = interval.as_secs() as i64;
    let sessions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM sessions WHERE volume_shared AND archived_at IS NULL AND state <> 'reaped' \
           AND (last_disk_backup_at IS NULL OR last_disk_backup_at < now() - make_interval(secs => $1))",
    )
    .bind(secs as f64)
    .fetch_all(pool)
    .await?;
    let machines: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM machines WHERE volume_shared AND state <> 'destroyed' \
           AND (last_disk_backup_at IS NULL OR last_disk_backup_at < now() - make_interval(secs => $1))",
    )
    .bind(secs as f64)
    .fetch_all(pool)
    .await?;
    Ok(sessions.into_iter().map(Subject::Session).chain(machines.into_iter().map(Subject::Machine)).collect())
}

/// Back up whatever is due, forever, on whichever controld holds the lock.
pub fn spawn(state: AppState) {
    let Some(shared) = state.shared_volumes.clone() else { return };
    let Some(policy) = shared.backup.clone() else { return };
    if state.blobs.is_none() || state.secrets.is_none() {
        tracing::warn!("disk backups need object storage (PUKU_R2_*) and PUKU_SECRET_KEY; not running");
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(policy.scan);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut lock = crate::leases::SweepLock::with_key(DISK_BACKUP_LOCK_KEY);
        loop {
            tick.tick().await;
            if !lock.hold(&state.pool).await {
                continue;
            }
            let Some(b) = Backups::from_state(&state, policy.tmp_dir.clone()) else { continue };
            let subjects = match due(&state.pool, policy.interval).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "listing disks due for backup failed");
                    continue;
                }
            };
            for s in subjects {
                if let Err(e) = b.backup_once(s).await {
                    tracing::error!(subject = ?s, error = format!("{e:#}"), "disk backup failed");
                }
            }
        }
    });
}

/// Advisory-lock key for the disk backup leader ("pukudbak").
const DISK_BACKUP_LOCK_KEY: i64 = 0x7075_6b75_6462_616b;

/// Before a shared disk is used: rebuild it if its image is gone. `Ok(false)`
/// means it is gone and there is nothing to rebuild it from -- the caller
/// must not let a worker create an empty one in its place.
pub async fn ensure_disk(state: &AppState, subject: Subject) -> Result<bool> {
    let Some(shared) = state.shared_volumes.as_ref() else { return Ok(true) };
    let tmp = shared.backup.as_ref().map(|p| p.tmp_dir.clone()).unwrap_or_else(|| BackupPolicy::default().tmp_dir);
    match Backups::from_state(state, tmp) {
        Some(b) => Ok(b.restore_if_missing(subject).await? != RestoreResult::NoBackup),
        None => shared.admin.exists(&subject.image()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_refuse_tampering() {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&[7u8; 32]));
        let id = Uuid::new_v4();
        let sealed = seal(&cipher, id, 3, true, b"disk bytes").unwrap();
        assert_eq!(open(&cipher, id, 3, &sealed).unwrap(), (b"disk bytes".to_vec(), true));
        assert!(open(&cipher, id, 4, &sealed).is_err(), "a moved frame does not open");
        assert!(open(&cipher, Uuid::new_v4(), 3, &sealed).is_err(), "another backup's frame does not open");
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert!(open(&cipher, id, 3, &bad).is_err(), "an altered frame does not open");
    }
}
