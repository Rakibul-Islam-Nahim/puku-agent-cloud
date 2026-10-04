//! Snapshots on the worker: capturing a machine's disks to object storage,
//! and laying them back down before a restored boot (docs/MACHINES-API.md,
//! "Snapshots").
//!
//! A layer is `tar | zstd | seal` ([`codec`]), cut into equal parts and PUT
//! to URLs controld presigns: this worker never holds a bucket key. The
//! packing runs on a blocking thread and the uploads on the runtime, one
//! part in flight while the next is compressed.
//!
//! A capture never blocks the machine's next boot for longer than it takes
//! to read the root disk, which the boot is about to write to. Whatever it
//! reads from the volume after that is "live": still a valid snapshot, just
//! not a clean one.

pub mod codec;

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::{Buf, Bytes};
use puku_cloud_proto::machine::MachineSpec;
use puku_cloud_proto::snapshot::{
    Consistency, Fingerprint, LayerOrder, PartDone, PartUrl, SnapshotLayer, SnapshotOrder, SnapshotProgress,
};
use puku_cloud_proto::worker_proto::Up;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use uuid::Uuid;

use crate::machines::Running;
use crate::vm::ExecRequest;
use codec::{OpenReader, PartSink, SealWriter};

/// Part URLs asked for at once.
const URL_BATCH: u32 = 16;
/// How long to wait on controld. Frames queue while the control link
/// reconnects, so this is generous.
const BROKER_WAIT: Duration = Duration::from_secs(120);
const PUT_ATTEMPTS: u32 = 5;
const RESTORE_ATTEMPTS: u32 = 3;

/// How hard this worker works at snapshots.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub zstd_level: i32,
    /// Captures and restores at once. Each holds up to two parts in memory.
    pub concurrency: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { zstd_level: 3, concurrency: 2 }
    }
}

/// One capture in flight, as the machine's lifecycle sees it.
pub struct Capture {
    /// The machine's VM ran, or started, while this read its disks: the
    /// result is live, whatever the order assumed.
    live: AtomicBool,
    root_read: watch::Sender<bool>,
    finished: watch::Sender<bool>,
}

impl Capture {
    fn new(live: bool) -> Self {
        Capture { live: AtomicBool::new(live), root_read: watch::channel(false).0, finished: watch::channel(false).0 }
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }
}

type UrlWaiter = oneshot::Sender<std::result::Result<Vec<PartUrl>, String>>;
type GetWaiter = oneshot::Sender<Option<String>>;

#[derive(Clone)]
pub struct Snapshots {
    inner: Arc<Inner>,
}

struct Inner {
    /// `<state_dir>/machines`.
    root: PathBuf,
    up_tx: mpsc::UnboundedSender<Up>,
    http: reqwest::Client,
    settings: Settings,
    permits: Semaphore,
    urls: Mutex<HashMap<(Uuid, SnapshotLayer, u32), UrlWaiter>>,
    gets: Mutex<HashMap<(Uuid, SnapshotLayer), GetWaiter>>,
    active: Mutex<HashMap<Uuid, Arc<Capture>>>,
}

/// What a capture came to.
enum Outcome {
    Skipped(Fingerprint),
    Ready { consistency: Consistency, fingerprint: Fingerprint, reused: Vec<SnapshotLayer> },
}

/// What tar reads for one layer.
enum Source {
    Dir { dir: PathBuf, excludes: Vec<String> },
    File { dir: PathBuf, name: &'static str },
}

struct LayerSummary {
    plain_bytes: u64,
    stored_bytes: u64,
    sha256: String,
}

impl Snapshots {
    pub fn new(root: PathBuf, up_tx: mpsc::UnboundedSender<Up>, settings: Settings) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Snapshots {
            inner: Arc::new(Inner {
                root,
                up_tx,
                http,
                permits: Semaphore::new(settings.concurrency.max(1)),
                settings,
                urls: Mutex::new(HashMap::new()),
                gets: Mutex::new(HashMap::new()),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    // -------------------------------------------------- controld's answers

    /// Resolve a `Down::SnapshotUrls`. Unknown answers are dropped: a late
    /// or duplicate one must not panic the link.
    pub fn resolve_urls(
        &self,
        snapshot_id: Uuid,
        layer: SnapshotLayer,
        first_part: u32,
        urls: Vec<PartUrl>,
        error: Option<String>,
    ) {
        if let Some(tx) = self.inner.urls.lock().unwrap().remove(&(snapshot_id, layer, first_part)) {
            let _ = tx.send(match error {
                Some(e) => Err(e),
                None => Ok(urls),
            });
        }
    }

    pub fn resolve_get(&self, snapshot_id: Uuid, layer: SnapshotLayer, url: Option<String>) {
        if let Some(tx) = self.inner.gets.lock().unwrap().remove(&(snapshot_id, layer)) {
            let _ = tx.send(url);
        }
    }

    async fn part_urls(&self, snapshot_id: Uuid, layer: SnapshotLayer, first_part: u32, count: u32) -> Result<Vec<PartUrl>> {
        let (tx, rx) = oneshot::channel();
        self.inner.urls.lock().unwrap().insert((snapshot_id, layer, first_part), tx);
        let _ = self.inner.up_tx.send(Up::RequestSnapshotUrls { snapshot_id, layer, first_part, count });
        match tokio::time::timeout(BROKER_WAIT, rx).await {
            Ok(Ok(Ok(urls))) => Ok(urls),
            Ok(Ok(Err(e))) => bail!("controld refused the upload: {e}"),
            _ => {
                self.inner.urls.lock().unwrap().remove(&(snapshot_id, layer, first_part));
                bail!("controld did not answer with upload URLs")
            }
        }
    }

    async fn get_url(&self, snapshot_id: Uuid, layer: SnapshotLayer) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.inner.gets.lock().unwrap().insert((snapshot_id, layer), tx);
        let _ = self.inner.up_tx.send(Up::RequestSnapshotGet { snapshot_id, layer });
        match tokio::time::timeout(BROKER_WAIT, rx).await {
            Ok(Ok(Some(url))) => Ok(url),
            Ok(Ok(None)) => bail!("controld refused the download"),
            _ => {
                self.inner.gets.lock().unwrap().remove(&(snapshot_id, layer));
                bail!("controld did not answer with a download URL")
            }
        }
    }

    // ----------------------------------------------------------- captures

    /// Register a capture of `machine_id` before it starts, so a boot queued
    /// behind it knows to wait for the root disk. `clean` when the machine's
    /// VM is not running.
    pub fn begin(&self, machine_id: Uuid, clean: bool) -> Arc<Capture> {
        let cap = Arc::new(Capture::new(!clean));
        self.inner.active.lock().unwrap().insert(machine_id, cap.clone());
        cap
    }

    fn current(&self, machine_id: Uuid) -> Option<Arc<Capture>> {
        self.inner.active.lock().unwrap().get(&machine_id).cloned()
    }

    /// Called before a machine boots: a capture of it that is still reading
    /// is from here on live, and the boot waits until its root disk is read.
    pub async fn before_boot(&self, machine_id: Uuid) {
        if let Some(cap) = self.current(machine_id) {
            cap.live.store(true, Ordering::SeqCst);
            let _ = cap.root_read.subscribe().wait_for(|read| *read).await;
        }
    }

    /// Wait out any capture of `machine_id`: before its disks are deleted or
    /// replaced by a restore.
    pub async fn wait_idle(&self, machine_id: Uuid) {
        if let Some(cap) = self.current(machine_id) {
            let _ = cap.finished.subscribe().wait_for(|done| *done).await;
        }
    }

    /// Run an ordered capture to its end and report how it went. `running`
    /// is the machine's VM when it is up: the guest is asked to flush first.
    pub async fn capture(&self, order: SnapshotOrder, cap: Arc<Capture>, running: Option<Arc<Running>>) {
        let outcome = async {
            let _permit = self.inner.permits.acquire().await.context("the worker is shutting down")?;
            self.run(&order, &cap, running.as_deref()).await
        }
        .await;
        let (state, consistency, error, fingerprint, reused) = match outcome {
            Ok(Outcome::Skipped(fp)) => (SnapshotProgress::Skipped, None, None, Some(fp), Vec::new()),
            Ok(Outcome::Ready { consistency, fingerprint, reused }) => {
                (SnapshotProgress::Ready, Some(consistency), None, Some(fingerprint), reused)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(machine = %order.machine_id, snapshot = %order.snapshot_id, error = %msg, "snapshot failed");
                (SnapshotProgress::Failed, None, Some(msg), None, Vec::new())
            }
        };
        let _ = self.inner.up_tx.send(Up::SnapshotState {
            snapshot_id: order.snapshot_id,
            machine_id: order.machine_id,
            state,
            consistency,
            error,
            fingerprint,
            reused,
        });
        cap.root_read.send_replace(true);
        cap.finished.send_replace(true);
        let mut active = self.inner.active.lock().unwrap();
        if active.get(&order.machine_id).is_some_and(|c| Arc::ptr_eq(c, &cap)) {
            active.remove(&order.machine_id);
        }
    }

    async fn run(&self, order: &SnapshotOrder, cap: &Arc<Capture>, running: Option<&Running>) -> Result<Outcome> {
        let dir = self.inner.root.join(order.machine_id.to_string());
        anyhow::ensure!(dir.is_dir(), "this worker holds no copy of machine {}", order.machine_id);
        let key = parse_key(&order.key_hex)?;
        let volume = dir.join("volume");
        let root = dir.join("root.ext4");
        if let Some(r) = running {
            // Flush the guest's page cache, so a live capture reads what the
            // guest believes it wrote.
            let sync = r.vm.exec(ExecRequest { cmd: "sync".into(), user: Some("root".into()), ..Default::default() });
            if !matches!(tokio::time::timeout(Duration::from_secs(30), sync).await, Ok(Ok(o)) if o.success()) {
                tracing::debug!(machine = %order.machine_id, "the guest did not sync before a live snapshot");
            }
        }
        let fp = {
            let (v, r, x) = (volume.clone(), root.clone(), order.excludes.clone());
            tokio::task::spawn_blocking(move || fingerprint(&v, &r, &x)).await?
        };
        if order.previous.as_ref() == Some(&fp) {
            return Ok(Outcome::Skipped(fp));
        }
        let _ = self.inner.up_tx.send(Up::SnapshotState {
            snapshot_id: order.snapshot_id,
            machine_id: order.machine_id,
            state: SnapshotProgress::Uploading,
            consistency: None,
            error: None,
            fingerprint: None,
            reused: Vec::new(),
        });

        let mut reused = Vec::new();
        // The root disk first: a boot waiting on this machine may start as
        // soon as it has been read.
        if let Some(layer) = order.layers.iter().find(|l| l.layer == SnapshotLayer::Root) {
            let unchanged = order.previous.as_ref().is_some_and(|p| p.root.is_some() && p.root == fp.root);
            // A running VM's root disk is never read -- ext4 mid-write is a
            // torn image -- and an unchanged one need not be: the last
            // snapshot already holds it.
            if cap.is_live() || unchanged || !root.is_file() {
                reused.push(SnapshotLayer::Root);
            } else {
                let source = Source::File { dir: dir.clone(), name: "root.ext4" };
                self.upload_layer(order, layer, source, key, cap.clone()).await?;
            }
        }
        cap.root_read.send_replace(true);

        if let Some(layer) = order.layers.iter().find(|l| l.layer == SnapshotLayer::Volume) {
            anyhow::ensure!(volume.is_dir(), "machine {} has no volume on this worker", order.machine_id);
            let source = Source::Dir { dir: volume.clone(), excludes: order.excludes.clone() };
            self.upload_layer(order, layer, source, key, cap.clone()).await?;
        }
        let consistency = if cap.is_live() { Consistency::Live } else { Consistency::Clean };
        Ok(Outcome::Ready { consistency, fingerprint: fp, reused })
    }

    /// Pack one layer and upload it part by part, then say so.
    async fn upload_layer(
        &self,
        order: &SnapshotOrder,
        layer: &LayerOrder,
        source: Source,
        key: [u8; 32],
        cap: Arc<Capture>,
    ) -> Result<()> {
        let part_bytes = order.part_bytes.max(1) as usize;
        let level = self.inner.settings.zstd_level;
        let aad = aad(order.snapshot_id, layer.layer);
        // One part queued while another uploads: memory stays near two parts.
        let (tx, mut rx) = mpsc::channel::<(u32, Vec<u8>)>(1);
        let producer = tokio::task::spawn_blocking(move || produce(source, part_bytes, level, key, &aad, tx, &cap));

        let mut parts = Vec::new();
        let mut urls: HashMap<u32, String> = HashMap::new();
        let uploaded: Result<()> = async {
            while let Some((part, bytes)) = rx.recv().await {
                let url = match urls.remove(&part) {
                    Some(url) => url,
                    None => {
                        for u in self.part_urls(order.snapshot_id, layer.layer, part, URL_BATCH).await? {
                            urls.insert(u.part, u.url);
                        }
                        urls.remove(&part).with_context(|| format!("controld sent no URL for part {part}"))?
                    }
                };
                let len = bytes.len() as u64;
                let etag = self.put_part(order.snapshot_id, layer.layer, part, url, Bytes::from(bytes)).await?;
                parts.push(PartDone { part, etag, bytes: len });
            }
            Ok(())
        }
        .await;
        if let Err(e) = uploaded {
            // The producer's next send fails and it stops tar.
            drop(rx);
            let _ = producer.await;
            return Err(e);
        }
        let summary = producer.await.context("the snapshot writer panicked")??;
        tracing::info!(
            snapshot = %order.snapshot_id, layer = layer.layer.as_str(), plain = summary.plain_bytes,
            stored = summary.stored_bytes, parts = parts.len(), "snapshot layer uploaded"
        );
        let _ = self.inner.up_tx.send(Up::SnapshotLayerDone {
            snapshot_id: order.snapshot_id,
            layer: layer.layer,
            parts,
            plain_bytes: summary.plain_bytes,
            stored_bytes: summary.stored_bytes,
            sha256: summary.sha256,
        });
        Ok(())
    }

    async fn put_part(
        &self,
        snapshot_id: Uuid,
        layer: SnapshotLayer,
        part: u32,
        mut url: String,
        body: Bytes,
    ) -> Result<String> {
        let mut delay = Duration::from_millis(500);
        let mut last = String::new();
        for attempt in 1..=PUT_ATTEMPTS {
            match self.inner.http.put(&url).body(body.clone()).send().await {
                Ok(r) if r.status().is_success() => {
                    return r
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                        .context("the bucket answered without an ETag");
                }
                // Most likely an expired URL: the part outlived its signature.
                Ok(r) if r.status() == reqwest::StatusCode::FORBIDDEN && attempt < PUT_ATTEMPTS => {
                    last = r.status().to_string();
                    if let Some(fresh) = self.part_urls(snapshot_id, layer, part, 1).await?.into_iter().next() {
                        url = fresh.url;
                    }
                }
                Ok(r) => last = r.status().to_string(),
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(8));
        }
        bail!("uploading part {part} failed after {PUT_ATTEMPTS} attempts: {last}")
    }

    /// Tell controld an ordered capture will not happen.
    pub fn refuse(&self, order: &SnapshotOrder, why: &str) {
        let _ = self.inner.up_tx.send(Up::SnapshotState {
            snapshot_id: order.snapshot_id,
            machine_id: order.machine_id,
            state: SnapshotProgress::Failed,
            consistency: None,
            error: Some(why.to_string()),
            fingerprint: None,
            reused: Vec::new(),
        });
    }

    // ----------------------------------------------------------- restores

    /// Lay a snapshot's disks down in `dir` (the machine's directory) before
    /// it boots. Each layer is unpacked beside the old one and swapped in
    /// only once it is whole.
    pub async fn restore(&self, spec: &MachineSpec, dir: &Path) -> Result<()> {
        let order = spec.restore.as_ref().context("nothing to restore")?;
        let key = parse_key(&order.key_hex)?;
        std::fs::create_dir_all(dir).context("creating the machine directory")?;
        let _permit = self.inner.permits.acquire().await.context("the worker is shutting down")?;
        for layer in &order.layers {
            let staged = dir.join(format!(".{}.restoring", layer.layer.as_str()));
            let mut attempt = 0;
            loop {
                attempt += 1;
                let _ = std::fs::remove_dir_all(&staged);
                std::fs::create_dir_all(&staged)?;
                let fetched = async {
                    let url = self.get_url(order.snapshot_id, layer.layer).await?;
                    self.download(&url, &staged, key, aad(order.snapshot_id, layer.layer), layer.sha256.as_deref()).await
                }
                .await;
                match fetched {
                    Ok(()) => break,
                    Err(e) if attempt < RESTORE_ATTEMPTS => {
                        tracing::warn!(machine = %spec.machine_id, layer = layer.layer.as_str(), attempt, error = format!("{e:#}"), "restoring a layer failed; retrying");
                        tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                    }
                    Err(e) => {
                        let _ = std::fs::remove_dir_all(&staged);
                        return Err(e.context(format!("restoring the {} layer", layer.layer.as_str())));
                    }
                }
            }
            match layer.layer {
                SnapshotLayer::Volume => swap_in(&staged, &dir.join("volume"))?,
                SnapshotLayer::Root => {
                    let disk = staged.join("root.ext4");
                    anyhow::ensure!(disk.is_file(), "the root layer holds no root.ext4");
                    std::fs::rename(&disk, dir.join("root.ext4"))?;
                    // The disk belongs to the image it was captured under
                    // (see `Machines::boot`).
                    std::fs::write(dir.join("root.image"), &spec.image)?;
                    let _ = std::fs::remove_dir_all(&staged);
                }
            }
            tracing::info!(machine = %spec.machine_id, snapshot = %order.snapshot_id, layer = layer.layer.as_str(), "layer restored");
        }
        Ok(())
    }

    async fn download(&self, url: &str, dest: &Path, key: [u8; 32], aad: Vec<u8>, sha256: Option<&str>) -> Result<()> {
        let mut resp = self.inner.http.get(url).send().await.context("fetching the layer")?;
        anyhow::ensure!(resp.status().is_success(), "fetching the layer: {}", resp.status());
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let dest = dest.to_path_buf();
        let unpacker = tokio::task::spawn_blocking(move || unpack(rx, &dest, key, &aad));
        let mut hasher = Sha256::new();
        let fetched: Result<()> = async {
            while let Some(chunk) = resp.chunk().await.context("reading the layer")? {
                hasher.update(&chunk);
                if tx.send(chunk).await.is_err() {
                    break; // the unpacker stopped; its error says why
                }
            }
            Ok(())
        }
        .await;
        drop(tx);
        let unpacked = unpacker.await.context("the snapshot reader panicked")?;
        fetched?;
        unpacked?;
        if let Some(want) = sha256 {
            let got = hex::encode(hasher.finalize());
            anyhow::ensure!(got == want, "the layer's checksum does not match: got {got}, want {want}");
        }
        Ok(())
    }
}

/// Binds a sealed layer to its snapshot and name.
fn aad(snapshot_id: Uuid, layer: SnapshotLayer) -> Vec<u8> {
    format!("puku-snapshot/{snapshot_id}/{}", layer.as_str()).into_bytes()
}

fn parse_key(key_hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(key_hex).context("the snapshot key is not hex")?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("the snapshot key is not 32 bytes"))
}

/// GNU tar leaves extended attributes out unless told to. libkrun's shared
/// folders keep the owner and mode the guest sees in `user.msb.override_stat`,
/// not in the host file's own metadata (which stays root's), so a volume
/// captured without them comes back root-only inside the guest.
const XATTR_ARGS: [&str; 2] = ["--xattrs", "--xattrs-include=user.*"];

fn tar_create(source: &Source) -> Command {
    let mut cmd = Command::new("tar");
    cmd.arg("--numeric-owner");
    if cfg!(target_os = "linux") {
        // GNU tar: holes stay holes, so a 16 GiB sparse disk costs what is
        // actually written.
        cmd.arg("--sparse");
        cmd.args(XATTR_ARGS);
    }
    match source {
        Source::Dir { dir, excludes } => {
            cmd.arg("-C").arg(dir);
            // Before the path: BSD tar applies --exclude only to what follows.
            for e in excludes {
                cmd.arg(format!("--exclude=./{}", e.trim_start_matches("./")));
            }
            if cfg!(target_os = "linux") {
                cmd.arg("--warning=no-file-changed");
            }
            cmd.args(["-cf", "-", "."]);
        }
        Source::File { dir, name } => {
            cmd.arg("-C").arg(dir).args(["-cf", "-", name]);
        }
    }
    cmd
}

/// tar | count | zstd | seal | parts, on a blocking thread. Returns once
/// every part has been handed to the uploader.
fn produce(
    source: Source,
    part_bytes: usize,
    level: i32,
    key: [u8; 32],
    aad: &[u8],
    tx: mpsc::Sender<(u32, Vec<u8>)>,
    cap: &Capture,
) -> Result<LayerSummary> {
    let mut child = tar_create(&source)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting tar")?;
    let stdout = child.stdout.take().context("tar has no stdout")?;
    let mut stderr = child.stderr.take().context("tar has no stderr")?;
    // Drained on its own thread: a full stderr pipe would stall tar, and so
    // this thread, and so the upload.
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let packed = (|| -> io::Result<(codec::SinkSummary, u64)> {
        let mut counting = Counting { inner: stdout, count: 0 };
        let sink = PartSink::new(part_bytes, move |part, bytes| {
            tx.blocking_send((part, bytes)).map_err(|_| io::Error::other("the upload stopped"))
        });
        let mut z = zstd::stream::write::Encoder::new(SealWriter::new(sink, &key, aad)?, level)?;
        io::copy(&mut counting, &mut z)?;
        let summary = z.finish()?.finish()?.finish()?;
        Ok((summary, counting.count))
    })();
    let (summary, plain_bytes) = match packed {
        Ok(v) => v,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::Error::new(e).context("packing the layer"));
        }
    };
    let status = child.wait().context("waiting for tar")?;
    let errors = errors.join().unwrap_or_default();
    if cfg!(target_os = "linux") && status.code() == Some(1) {
        // GNU tar's "some files changed as we read them": still a snapshot,
        // but a live one.
        cap.live.store(true, Ordering::SeqCst);
    } else if !status.success() {
        bail!("tar failed ({status}): {}", errors.trim());
    }
    Ok(LayerSummary { plain_bytes, stored_bytes: summary.bytes, sha256: summary.sha256 })
}

/// Sealed bytes from the download, into tar, on a blocking thread.
fn unpack(rx: mpsc::Receiver<Bytes>, dest: &Path, key: [u8; 32], aad: &[u8]) -> Result<()> {
    let open = OpenReader::new(ChannelReader { rx, cur: Bytes::new() }, &key, aad)?;
    let mut plain = zstd::stream::read::Decoder::new(open)?;
    let mut tar = Command::new("tar");
    tar.arg("--numeric-owner");
    if cfg!(target_os = "linux") {
        tar.args(XATTR_ARGS);
    }
    let mut child = tar
        .args(["-x", "-C"])
        .arg(dest)
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting tar")?;
    let mut stdin = child.stdin.take().context("tar has no stdin")?;
    let copied = io::copy(&mut plain, &mut stdin);
    drop(stdin);
    let out = child.wait_with_output().context("waiting for tar")?;
    // A decryption failure is the real error; tar's complaint about the
    // archive ending early is only its consequence.
    match copied {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
            return Err(anyhow::Error::new(e).context("reading the layer"));
        }
        _ => {}
    }
    if !out.status.success() {
        bail!("tar could not unpack the layer ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Put a restored directory where the live one was. The old one is renamed
/// aside first and deleted only once the new one is in place.
fn swap_in(staged: &Path, target: &Path) -> Result<()> {
    let old = target.with_extension("old");
    let _ = std::fs::remove_dir_all(&old);
    if target.exists() {
        std::fs::rename(target, &old).context("moving the old volume aside")?;
    }
    std::fs::rename(staged, target).context("moving the restored volume into place")?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// What a machine's disks look like, cheaply: each volume entry's path,
/// size, mtime and mode, and the root disk's size, mtime and allocated
/// blocks. Equal fingerprints mean nothing to upload.
pub fn fingerprint(volume: &Path, root: &Path, excludes: &[String]) -> Fingerprint {
    let volume_fp = volume.is_dir().then(|| {
        let mut h = Sha256::new();
        walk(volume, volume, excludes, &mut h);
        hex::encode(h.finalize())
    });
    let root_fp = std::fs::metadata(root)
        .ok()
        .map(|m| format!("{}:{}.{}:{}", m.len(), m.mtime(), m.mtime_nsec(), m.blocks()));
    Fingerprint { volume: volume_fp, root: root_fp }
}

fn walk(base: &Path, dir: &Path, excludes: &[String], h: &mut Sha256) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().into_owned();
        let excluded = excludes.iter().any(|x| {
            let x = x.trim_start_matches("./").trim_end_matches('/');
            rel == x || rel.starts_with(&format!("{x}/"))
        });
        if excluded {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(&path) else { continue };
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update(m.len().to_le_bytes());
        h.update(m.mtime().to_le_bytes());
        h.update(m.mtime_nsec().to_le_bytes());
        h.update(m.mode().to_le_bytes());
        h.update(b"\n");
        if m.is_dir() {
            walk(base, &path, excludes, h);
        }
    }
}

struct Counting<R> {
    inner: R,
    count: u64,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        Ok(n)
    }
}

/// A download, as the blocking unpacker reads it.
struct ChannelReader {
    rx: mpsc::Receiver<Bytes>,
    cur: Bytes,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                Some(b) => self.cur = b,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.cur.len());
        buf[..n].copy_from_slice(&self.cur[..n]);
        self.cur.advance(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puku_cloud_proto::snapshot::{RestoreOrder, SnapshotTrigger};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!("puku-snapshots-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Object path -> its parts, by part number.
    type Objects = Arc<Mutex<HashMap<String, BTreeMap<u32, Vec<u8>>>>>;

    /// A bucket that keeps what is PUT and serves it back: enough to run the
    /// whole pipeline end to end without S3.
    #[derive(Clone, Default)]
    struct Bucket {
        objects: Objects,
    }

    async fn serve(bucket: Bucket) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let bucket = bucket.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, bucket).await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn handle(sock: tokio::net::TcpStream, bucket: Bucket) -> io::Result<()> {
        let mut r = BufReader::new(sock);
        let mut line = String::new();
        r.read_line(&mut line).await?;
        let mut words = line.split_whitespace();
        let method = words.next().unwrap_or_default().to_string();
        let target = words.next().unwrap_or_default().to_string();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            r.read_line(&mut h).await?;
            if h == "\r\n" || h.is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let resp = match method.as_str() {
            "PUT" => {
                let part = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("partNumber="))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(1);
                bucket.objects.lock().unwrap().entry(path.to_string()).or_default().insert(part, body);
                format!("HTTP/1.1 200 OK\r\nETag: \"etag-{part}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .into_bytes()
            }
            "GET" => {
                let all: Vec<u8> = bucket
                    .objects
                    .lock()
                    .unwrap()
                    .get(path)
                    .map(|parts| parts.values().flatten().copied().collect())
                    .unwrap_or_default();
                let mut v = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", all.len())
                    .into_bytes();
                v.extend(all);
                v
            }
            _ => b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        };
        let mut sock = r.into_inner();
        sock.write_all(&resp).await?;
        sock.shutdown().await
    }

    /// Answers the worker the way controld would, until the capture reports
    /// how it ended; returns every frame it saw.
    fn stand_in(snaps: Snapshots, mut up_rx: mpsc::UnboundedReceiver<Up>, base: String) -> tokio::task::JoinHandle<Vec<Up>> {
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(frame) = up_rx.recv().await {
                let mut done = false;
                match &frame {
                    Up::RequestSnapshotUrls { snapshot_id, layer, first_part, count } => {
                        let urls = (*first_part..first_part + count)
                            .map(|part| PartUrl {
                                part,
                                url: format!("{base}/{snapshot_id}/{}?partNumber={part}&uploadId=u", layer.as_str()),
                            })
                            .collect();
                        snaps.resolve_urls(*snapshot_id, *layer, *first_part, urls, None);
                    }
                    Up::RequestSnapshotGet { snapshot_id, layer } => {
                        snaps.resolve_get(*snapshot_id, *layer, Some(format!("{base}/{snapshot_id}/{}", layer.as_str())));
                    }
                    Up::SnapshotState { state, .. } => done = *state != SnapshotProgress::Uploading,
                    _ => {}
                }
                seen.push(frame);
                if done {
                    break;
                }
            }
            seen
        })
    }

    /// Bytes zstd cannot shrink, so a small part size forces many parts.
    fn noise(len: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn spec(machine: Uuid, restore: Option<RestoreOrder>) -> MachineSpec {
        MachineSpec {
            machine_id: machine,
            name: puku_cloud_proto::machine::machine_name_for(machine),
            engine: puku_cloud_proto::Engine::Libkrun,
            image: "alpine".into(),
            cpus: 1,
            memory_mib: 512,
            expose: vec![],
            env: Default::default(),
            entrypoint: None,
            volume: Some(puku_cloud_proto::machine::VolumeSpec { path: "/home/u".into(), uid: 1000 }),
            max_duration_s: 0,
            generation: 2,
            persist_root: false,
            restore,
        }
    }

    #[tokio::test]
    async fn a_volume_is_captured_and_restored_elsewhere_byte_for_byte() {
        let root = scratch();
        let machine = Uuid::new_v4();
        let vol = root.join(machine.to_string()).join("volume");
        std::fs::create_dir_all(vol.join("docs/deep")).unwrap();
        std::fs::create_dir_all(vol.join(".cache")).unwrap();
        std::fs::write(vol.join("hello.txt"), b"hello").unwrap();
        let big = noise(300_000);
        std::fs::write(vol.join("docs/deep/big.bin"), &big).unwrap();
        std::fs::write(vol.join(".cache/junk"), b"regenerable").unwrap();

        let base = serve(Bucket::default()).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let snaps = Snapshots::new(root.clone(), tx, Settings::default());
        let answers = stand_in(snaps.clone(), rx, base.clone());
        let sid = Uuid::new_v4();
        let key_hex = hex::encode([9u8; 32]);
        let order = SnapshotOrder {
            snapshot_id: sid,
            machine_id: machine,
            trigger: SnapshotTrigger::Stop,
            key_hex: key_hex.clone(),
            part_bytes: 16 * 1024,
            layers: vec![LayerOrder { layer: SnapshotLayer::Volume, key: "k".into(), sha256: None }],
            excludes: vec![".cache".into()],
            previous: None,
        };
        snaps.capture(order.clone(), snaps.begin(machine, true), None).await;
        let frames = answers.await.unwrap();

        let (parts, sha) = frames
            .iter()
            .find_map(|f| match f {
                Up::SnapshotLayerDone { parts, sha256, .. } => Some((parts.clone(), sha256.clone())),
                _ => None,
            })
            .expect("the volume was uploaded");
        assert!(parts.len() > URL_BATCH as usize, "enough parts to need a second batch of URLs: {}", parts.len());
        assert!(parts.iter().enumerate().all(|(i, p)| p.part == i as u32 + 1), "parts are numbered in order");
        assert!(parts[..parts.len() - 1].iter().all(|p| p.bytes == 16 * 1024), "every part but the last is equal");
        let fingerprint = match frames.last().unwrap() {
            Up::SnapshotState { state, consistency, fingerprint, .. } => {
                assert_eq!(*state, SnapshotProgress::Ready);
                assert_eq!(*consistency, Some(Consistency::Clean), "the VM was not running");
                fingerprint.clone().unwrap()
            }
            other => panic!("the capture did not end with a state: {other:?}"),
        };

        // On "another worker": an empty state dir, and the same bucket.
        let elsewhere = scratch();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let snaps2 = Snapshots::new(elsewhere.clone(), tx2, Settings::default());
        let _answers2 = stand_in(snaps2.clone(), rx2, base.clone());
        let restore = RestoreOrder {
            snapshot_id: sid,
            key_hex: key_hex.clone(),
            layers: vec![LayerOrder { layer: SnapshotLayer::Volume, key: "k".into(), sha256: Some(sha) }],
        };
        let dir = elsewhere.join(machine.to_string());
        snaps2.restore(&spec(machine, Some(restore)), &dir).await.unwrap();
        assert_eq!(std::fs::read(dir.join("volume/hello.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dir.join("volume/docs/deep/big.bin")).unwrap(), big);
        assert!(!dir.join("volume/.cache/junk").exists(), "excluded paths are not captured");
        assert!(!dir.join(".volume.restoring").exists(), "the staging directory is gone");

        // Nothing changed: the next capture that may skip, does.
        let (tx3, rx3) = mpsc::unbounded_channel();
        let snaps3 = Snapshots::new(root.clone(), tx3, Settings::default());
        let answers3 = stand_in(snaps3.clone(), rx3, base);
        let again = SnapshotOrder { snapshot_id: Uuid::new_v4(), previous: Some(fingerprint), ..order };
        snaps3.capture(again, snaps3.begin(machine, true), None).await;
        let frames = answers3.await.unwrap();
        assert!(
            matches!(frames.last(), Some(Up::SnapshotState { state: SnapshotProgress::Skipped, .. })),
            "{frames:?}"
        );
        assert!(!frames.iter().any(|f| matches!(f, Up::SnapshotLayerDone { .. })), "nothing was uploaded");
    }

    /// A wrong key or a checksum mismatch fails the restore and leaves the
    /// volume that was there untouched.
    #[tokio::test]
    async fn a_restore_that_cannot_be_verified_changes_nothing() {
        let root = scratch();
        let machine = Uuid::new_v4();
        let vol = root.join(machine.to_string()).join("volume");
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::write(vol.join("a"), b"original").unwrap();

        let base = serve(Bucket::default()).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let snaps = Snapshots::new(root.clone(), tx, Settings::default());
        let answers = stand_in(snaps.clone(), rx, base.clone());
        let sid = Uuid::new_v4();
        let order = SnapshotOrder {
            snapshot_id: sid,
            machine_id: machine,
            trigger: SnapshotTrigger::Manual,
            key_hex: hex::encode([1u8; 32]),
            part_bytes: 1 << 20,
            layers: vec![LayerOrder { layer: SnapshotLayer::Volume, key: "k".into(), sha256: None }],
            excludes: vec![],
            previous: None,
        };
        snaps.capture(order, snaps.begin(machine, true), None).await;
        answers.await.unwrap();

        let (tx2, rx2) = mpsc::unbounded_channel();
        let snaps2 = Snapshots::new(root.clone(), tx2, Settings::default());
        let _answers2 = stand_in(snaps2.clone(), rx2, base);
        let restore = RestoreOrder {
            snapshot_id: sid,
            key_hex: hex::encode([2u8; 32]),
            layers: vec![LayerOrder { layer: SnapshotLayer::Volume, key: "k".into(), sha256: None }],
        };
        let dir = root.join(machine.to_string());
        std::fs::write(vol.join("a"), b"changed since").unwrap();
        assert!(snaps2.restore(&spec(machine, Some(restore)), &dir).await.is_err());
        assert_eq!(std::fs::read(vol.join("a")).unwrap(), b"changed since", "the live volume was not replaced");
    }

    /// libkrun keeps the guest's view of ownership in an extended attribute on
    /// the host file; a restore that drops it leaves the volume root-only in
    /// the guest, and the machine's own user locked out of its files.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn libkrun_ownership_attributes_survive_a_restore() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        const KEY: &[u8] = b"user.msb.override_stat\0";
        let set = |p: &Path, v: &[u8]| {
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            unsafe { libc::setxattr(c.as_ptr(), KEY.as_ptr().cast(), v.as_ptr().cast(), v.len(), 0) }
        };
        let get = |p: &Path| -> Option<Vec<u8>> {
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            let mut buf = vec![0u8; 256];
            let n = unsafe { libc::getxattr(c.as_ptr(), KEY.as_ptr().cast(), buf.as_mut_ptr().cast(), buf.len()) };
            (n >= 0).then(|| {
                buf.truncate(n as usize);
                buf
            })
        };

        let root = scratch();
        let machine = Uuid::new_v4();
        let vol = root.join(machine.to_string()).join("volume");
        std::fs::create_dir_all(vol.join("sub")).unwrap();
        std::fs::write(vol.join("sub/hello.txt"), b"hello volume").unwrap();
        if set(&vol.join("sub"), b"1000:1000:040755") != 0 || set(&vol.join("sub/hello.txt"), b"1000:1000:0100644") != 0 {
            eprintln!("skipped: this filesystem does not take user extended attributes");
            return;
        }

        let base = serve(Bucket::default()).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let snaps = Snapshots::new(root.clone(), tx, Settings::default());
        let answers = stand_in(snaps.clone(), rx, base.clone());
        let sid = Uuid::new_v4();
        let key_hex = hex::encode([7u8; 32]);
        let order = SnapshotOrder {
            snapshot_id: sid,
            machine_id: machine,
            trigger: SnapshotTrigger::Stop,
            key_hex: key_hex.clone(),
            part_bytes: 1 << 20,
            layers: vec![LayerOrder { layer: SnapshotLayer::Volume, key: "k".into(), sha256: None }],
            excludes: vec![],
            previous: None,
        };
        snaps.capture(order, snaps.begin(machine, true), None).await;
        let sha = answers
            .await
            .unwrap()
            .iter()
            .find_map(|f| match f {
                Up::SnapshotLayerDone { sha256, .. } => Some(sha256.clone()),
                _ => None,
            })
            .expect("the volume was uploaded");

        let elsewhere = scratch();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let snaps2 = Snapshots::new(elsewhere.clone(), tx2, Settings::default());
        let _answers2 = stand_in(snaps2.clone(), rx2, base);
        let restore = RestoreOrder {
            snapshot_id: sid,
            key_hex,
            layers: vec![LayerOrder { layer: SnapshotLayer::Volume, key: "k".into(), sha256: Some(sha) }],
        };
        let dir = elsewhere.join(machine.to_string());
        snaps2.restore(&spec(machine, Some(restore)), &dir).await.unwrap();
        assert_eq!(std::fs::read(dir.join("volume/sub/hello.txt")).unwrap(), b"hello volume");
        assert_eq!(get(&dir.join("volume/sub")).as_deref(), Some(&b"1000:1000:040755"[..]));
        assert_eq!(get(&dir.join("volume/sub/hello.txt")).as_deref(), Some(&b"1000:1000:0100644"[..]));
    }

    #[test]
    fn a_fingerprint_changes_with_the_files_but_not_the_excluded_ones() {
        let d = scratch();
        let vol = d.join("volume");
        std::fs::create_dir_all(vol.join(".cache")).unwrap();
        std::fs::write(vol.join("a"), b"1").unwrap();
        let x = vec![".cache".to_string()];
        let first = fingerprint(&vol, &d.join("root.ext4"), &x);
        assert!(first.root.is_none(), "no root disk, no root fingerprint");
        std::fs::write(vol.join(".cache/c"), b"noise").unwrap();
        assert_eq!(fingerprint(&vol, &d.join("root.ext4"), &x), first, "an excluded file changes nothing");
        std::fs::write(vol.join("b"), b"2").unwrap();
        assert_ne!(fingerprint(&vol, &d.join("root.ext4"), &x), first);
    }
}
