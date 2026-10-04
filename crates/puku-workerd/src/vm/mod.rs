//! The seam between a session actor and whatever boots its VM.
//!
//! Everything above this module -- the outbox, the stdin fifo, the launch
//! wrapper, reconcile, idle parking -- is engine-agnostic, and stays that way
//! by only ever talking to these two traits. Each engine implements them:
//!
//! * [`msb`] -- microsandbox (libkrun), the original and default engine.
//! * [`ch`]  -- Cloud Hypervisor, one VMM process per VM.
//!
//! A worker holds one [`Backends`] with whichever engines the operator
//! enabled, and each session actor is handed the backend its spec names.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use puku_cloud_proto::Engine;

pub mod ch;
pub mod msb;

#[cfg(test)]
pub mod fake;

/// A host directory shared into the guest at `guest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub guest: String,
    pub host: PathBuf,
}

/// A credential delivered at the network boundary rather than as plain env:
/// the guest sees a placeholder, and the real value is substituted only into
/// requests bound for `hosts` (`*.` prefixes are wildcards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEnv {
    pub var: String,
    pub value: String,
    pub hosts: Vec<String>,
}

/// Everything an engine needs to boot one VM. Engine-neutral on purpose: the
/// session actor fills this in once, and each backend maps it onto its own
/// mechanisms.
#[derive(Debug, Clone, Default)]
pub struct VmSpec {
    /// Unique on the host. Session VMs are `ses-<12 hex>`.
    pub name: String,
    /// OCI reference. How it becomes a root filesystem is the engine's
    /// business: msb pulls into its own store, Cloud Hypervisor reads a
    /// pre-staged disk image.
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub mounts: Vec<Mount>,
    pub labels: Vec<(String, String)>,
    /// Hard wall-clock cap after which the VM is torn down. `None` is none.
    pub max_duration_s: Option<u64>,
    pub env: Vec<(String, String)>,
    pub secrets: Vec<SecretEnv>,
    /// Domain-suffix allowlist. Empty attaches no policy at all, which means
    /// unrestricted egress -- see workerd's `--egress-unrestricted`.
    pub egress_allow: Vec<String>,
    /// Apply the engine's multi-tenant isolation floor.
    pub multi_tenant: bool,
    /// Guest TCP ports [`Vm::connect_port`] must be able to reach. Declared
    /// up front because some engines (msb) can only publish at create.
    pub ports: Vec<u16>,
    /// A writable root disk kept across boots instead of a fresh one each
    /// time (a machine's `persist_root`, Cloud Hypervisor). Formatted when it
    /// does not exist yet. Engines without one ignore it.
    pub root_disk: Option<PathBuf>,
    /// The guest's /dev/shm, where the engine sizes it; `None` is the
    /// engine's default.
    pub shm_mib: Option<u32>,
}

/// A byte pipe into the guest.
pub trait GuestIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> GuestIo for T {}

/// How a streaming exec ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamOutcome {
    pub code: i32,
    /// Collected, capped at `puku_cloud_proto::data_proto::MAX_EXEC_OUTPUT_BYTES`.
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// Exit code for a command killed by its timeout, as coreutils `timeout`
/// reports it -- the convention callers already test for.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

/// One command to run inside a VM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecRequest {
    pub cmd: String,
    pub args: Vec<String>,
    /// Bytes written to the command's stdin, which is then closed. `None`
    /// leaves stdin at the engine default (nothing).
    pub stdin: Option<Vec<u8>>,
    pub user: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl ExecRequest {
    /// `sh -c <script>`, which is how nearly every call in this crate runs.
    pub fn sh(script: impl Into<String>) -> Self {
        ExecRequest {
            cmd: "sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Default::default()
        }
    }

    pub fn with_stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The command was killed by `ExecRequest::timeout`; `code` is then
    /// [`TIMEOUT_EXIT_CODE`].
    pub timed_out: bool,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// A running VM this worker can talk to.
#[async_trait]
pub trait Vm: Send + Sync {
    fn name(&self) -> &str;
    /// Run a command to completion and collect its output.
    async fn exec(&self, req: ExecRequest) -> Result<ExecOutput>;
    /// Run a command with stdin fed from `stdin` (closed when that ends;
    /// `req.stdin` is ignored) and stdout forwarded chunk by chunk, for
    /// payloads too big to hold: files and tarballs. Stderr is collected.
    async fn exec_stream(
        &self,
        req: ExecRequest,
        stdin: Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
        stdout: tokio::sync::mpsc::Sender<bytes::Bytes>,
    ) -> Result<StreamOutcome>;
    /// A TCP connection to `127.0.0.1:port` inside the guest. The port must
    /// have been in `VmSpec::ports`.
    async fn connect_port(&self, port: u16) -> Result<Box<dyn GuestIo>>;
    /// Shut the guest down. Returning does not promise the engine has
    /// released everything yet; see [`remove_with_retry`].
    async fn stop(&self) -> Result<()>;
}

/// A boot failure with a reason the control plane can act on
/// (`image_not_staged`, `engine_unavailable`), carried through `anyhow` so
/// the machine's state report can name it instead of leaving the caller to
/// parse a sentence.
#[derive(Debug)]
pub struct BootRefusal {
    pub reason: &'static str,
    pub message: String,
}

impl BootRefusal {
    pub fn new(reason: &'static str, message: impl Into<String>) -> Self {
        BootRefusal { reason, message: message.into() }
    }
}

impl std::fmt::Display for BootRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BootRefusal {}

/// The reason code for a failed boot: a [`BootRefusal`] anywhere in the
/// chain, a full disk, or plain `boot_failed`.
pub fn boot_failure_reason(e: &anyhow::Error) -> &'static str {
    for cause in e.chain() {
        if let Some(r) = cause.downcast_ref::<BootRefusal>() {
            return r.reason;
        }
        if cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull) {
            return "insufficient_disk";
        }
    }
    "boot_failed"
}

/// One hypervisor, as this worker drives it.
#[async_trait]
pub trait VmBackend: Send + Sync {
    fn engine(&self) -> Engine;
    /// Reported to controld as the worker's toolchain version.
    fn version(&self) -> String;
    /// Boot a VM detached from this process: it must survive a workerd
    /// restart, which is what makes reconcile-from-disk possible.
    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>>;
    /// Reconnect to a VM that is still running after a workerd restart.
    async fn attach(&self, name: &str) -> Result<Box<dyn Vm>>;
    /// Remove a stopped VM and whatever the engine keeps for it. One attempt;
    /// callers that need it gone use [`remove_with_retry`].
    async fn remove(&self, name: &str) -> Result<()>;
    /// Every VM this engine has on the host, across all pages. Feeds the
    /// heartbeat inventory, whose whole purpose is to disagree with the
    /// database when something leaked -- a partial list hides exactly that.
    async fn list(&self) -> Result<Vec<String>>;
    /// Make `image` ready to boot so a first session does not pay for it.
    async fn prepull(&self, image: &str) -> Result<()>;
    /// Images staged ahead of time, for an engine that boots a prepared
    /// disk. `None` for one that pulls on demand (msb): there is nothing for
    /// controld to check before placing on it.
    fn staged_images(&self) -> Option<Vec<puku_cloud_proto::worker_proto::StagedImage>> {
        None
    }
}

/// Remove a VM, retrying while the engine finishes letting go of it.
///
/// `stop()` returns before msb has released everything, so a `remove` issued
/// straight after it loses the race and fails once -- and nothing ever tried
/// again. The failure was logged at `debug`, invisible under the deployment's
/// `RUST_LOG=info`, so the leak was silent: measured at roughly one sandbox
/// per twenty sessions, each pinning its image and showing up in
/// `/v1/fleet` drift with no operator-facing way to clear it.
///
/// Bounded, because a VM that genuinely no longer exists also returns an
/// error here, and retrying that forever would be its own bug. A final
/// failure is a `warn`, which is the level an operator actually sees.
pub async fn remove_with_retry(backend: &dyn VmBackend, name: &str, session_id: uuid::Uuid) {
    const ATTEMPTS: u32 = 5;
    let mut delay = Duration::from_millis(250);
    for attempt in 1..=ATTEMPTS {
        match backend.remove(name).await {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!(%session_id, sandbox = %name, attempt, "sandbox removed on retry");
                }
                return;
            }
            Err(e) if attempt == ATTEMPTS => {
                tracing::warn!(
                    %session_id, sandbox = %name, attempt, engine = %backend.engine(),
                    error = format!("{e:#}"),
                    "could not remove the sandbox; it will show as fleet drift until removed by hand"
                );
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
}

/// The engines this worker runs, keyed by [`Engine`].
#[derive(Clone, Default)]
pub struct Backends {
    libkrun: Option<Arc<dyn VmBackend>>,
    cloud_hypervisor: Option<Arc<dyn VmBackend>>,
}

impl Backends {
    pub fn with(mut self, backend: Arc<dyn VmBackend>) -> Self {
        match backend.engine() {
            Engine::Libkrun => self.libkrun = Some(backend),
            Engine::CloudHypervisor => self.cloud_hypervisor = Some(backend),
            Engine::Unsupported => {}
        }
        self
    }

    pub fn get(&self, engine: Engine) -> Option<Arc<dyn VmBackend>> {
        match engine {
            Engine::Libkrun => self.libkrun.clone(),
            Engine::CloudHypervisor => self.cloud_hypervisor.clone(),
            Engine::Unsupported => None,
        }
    }

    pub fn all(&self) -> Vec<Arc<dyn VmBackend>> {
        [&self.libkrun, &self.cloud_hypervisor].into_iter().flatten().cloned().collect()
    }

    /// What this worker advertises in `Register`.
    pub fn engines(&self) -> Vec<Engine> {
        self.all().iter().map(|b| b.engine()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.libkrun.is_none() && self.cloud_hypervisor.is_none()
    }

    /// Every engine's staged images, or `None` when no engine stages any.
    pub fn staged_images(&self) -> Option<Vec<puku_cloud_proto::worker_proto::StagedImage>> {
        let mut out: Option<Vec<_>> = None;
        for b in self.all() {
            if let Some(mut images) = b.staged_images() {
                out.get_or_insert_with(Vec::new).append(&mut images);
            }
        }
        out
    }

    /// `libkrun=msb-0.6.9,cloud_hypervisor=v45.0` -- one string, because the
    /// column it lands in predates there being more than one toolchain.
    pub fn version_string(&self) -> String {
        self.all()
            .iter()
            .map(|b| format!("{}={}", b.engine(), b.version()))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Every VM name across every engine, for the heartbeat. Best effort per
    /// engine: one failing listing must not blank out the others, and must
    /// never break the heartbeat, which is also the liveness signal.
    pub async fn inventory(&self) -> Vec<String> {
        let mut names = Vec::new();
        for b in self.all() {
            match b.list().await {
                Ok(mut n) => names.append(&mut n),
                Err(e) => {
                    tracing::debug!(engine = %b.engine(), error = format!("{e:#}"), "listing host VMs failed")
                }
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_advertise_exactly_what_they_hold() {
        let b = Backends::default();
        assert!(b.is_empty());
        assert!(b.engines().is_empty());
        let b = b.with(Arc::new(fake::FakeBackend::new(Engine::Libkrun)));
        assert_eq!(b.engines(), vec![Engine::Libkrun]);
        assert!(b.get(Engine::CloudHypervisor).is_none());
        let b = b.with(Arc::new(fake::FakeBackend::new(Engine::CloudHypervisor)));
        assert_eq!(b.engines(), vec![Engine::Libkrun, Engine::CloudHypervisor]);
        assert!(b.version_string().contains("cloud_hypervisor="));
    }

    #[test]
    fn sh_builds_the_shape_every_caller_uses() {
        let r = ExecRequest::sh("echo hi").with_stdin("x");
        assert_eq!(r.cmd, "sh");
        assert_eq!(r.args, vec!["-c".to_string(), "echo hi".to_string()]);
        assert_eq!(r.stdin.as_deref(), Some(&b"x"[..]));
    }

    /// Remove must be retried past the transient failure msb returns while a
    /// stopped VM is still being released.
    #[tokio::test(start_paused = true)]
    async fn remove_is_retried_past_a_transient_failure() {
        let b = fake::FakeBackend::new(Engine::Libkrun);
        b.fail_removes(2);
        remove_with_retry(&b, "ses-x", uuid::Uuid::nil()).await;
        assert_eq!(b.remove_attempts(), 3, "two failures then success");
    }

    /// ...and give up on a permanent one rather than hang the actor.
    #[tokio::test(start_paused = true)]
    async fn remove_gives_up_after_a_bounded_number_of_attempts() {
        let b = fake::FakeBackend::new(Engine::Libkrun);
        b.fail_removes(100);
        remove_with_retry(&b, "ses-x", uuid::Uuid::nil()).await;
        assert_eq!(b.remove_attempts(), 5, "bounded");
    }
}
