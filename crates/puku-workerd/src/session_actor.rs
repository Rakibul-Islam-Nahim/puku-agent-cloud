//! One actor per assigned session: prepares volumes, boots the microVM,
//! starts the in-guest runner, tails the event outbox, and relays input.
//!
//! Sandboxes are created detached and the spec is persisted next to the
//! session volumes, so a restarted workerd can reconcile: reattach to the
//! still-running VM, re-tail the outbox (controld dedups by guest line),
//! and keep driving input. Runner exit is always observable from the
//! `exec.exited` marker the runner appends to the outbox, so no live
//! ExecHandle is required after a restart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use puku_cloud_proto::event::{EventKind, GuestEvent};
use puku_cloud_proto::session::{GuestManifest, SessionSpec, SessionState};
use puku_cloud_proto::worker_proto::{Up, MAX_EVENTS_PER_FRAME};
use tokio::sync::{mpsc, watch};

use crate::controlplane::SessionMap;
use crate::vm::{ExecRequest, Mount, SecretEnv, Vm, VmBackend, VmSpec};

/// What the guest runs when no `PUKU_RUNNER_CMD` override is set.
///
/// The SDK runner. `sdk-gate.sh` passes all four items against a real
/// deployment, and `feature-matrix.sh` is at parity with the bash runner.
///
/// It was briefly made the default and reverted on a gate FAIL that turned
/// out to be the gate's own race -- it grepped for the agent's reply the
/// instant the answer was *delivered*, before the model had written one. The
/// runner had been delivering answers correctly the whole time.
///
/// The reason to prefer it is `AskUserQuestion`: the bash runner cannot route
/// it on puku-cli 1.8.48. `--permission-prompt-tool stdio` is still accepted
/// but silently stopped routing that one tool, so the agent asks, nothing
/// reaches the outbox, and the CLI answers itself. The SDK runner owns the
/// control channel through `canUseTool` and does not depend on that flag.
///
/// The bash runner stays in the image; `PUKU_RUNNER_CMD='exec
/// /usr/local/bin/puku-runner'` selects it again with a restart, no rebuild.
/// Anything containing `runner.mjs` is treated as the SDK runner by
/// `uses_sdk_runner`, which decides how an interrupt is delivered -- that
/// distinction is load-bearing whichever way the default points.
pub const DEFAULT_RUNNER_CMD: &str = "exec node /opt/puku/runner.mjs";

#[derive(Debug)]
pub enum InputCmd {
    /// A puku-cli stream-json line to append to the guest stdin fifo.
    Line(String),
    Interrupt,
    Park,
    Kill,
}

/// How the session ended, decided by the actor's main loop.
enum Outcome {
    Exited(i32),
    Parked,
    Killed,
    Error(String),
    /// The watchdog: the VM died or stopped answering.
    Crashed(String),
}

/// What the tailer needs from the actor: the signals it raises, and the
/// handles it uses to ship oversized payloads off the box.
struct TailSignals {
    /// Fires with the exit code when the `exec.exited` marker line appears,
    /// and when the agent emits a terminal `result` -- see `inspect_line`.
    exit_tx: mpsc::UnboundedSender<i32>,
    /// The agent's own words for why the turn failed. `exit_tx` can only
    /// carry a number, and "runner exited with code 1" tells an operator
    /// nothing when the real answer is a quota or an auth error.
    agent_error: Arc<Mutex<Option<String>>>,
    /// Bumped on every new outbox line (idle detection).
    activity: Arc<Mutex<Instant>>,
    /// Set while the agent is blocked on a question (idle timer backs off).
    pending_question: Arc<AtomicBool>,
    /// Set when the platform asked for an interrupt and the agent has not
    /// yet ended the turn it was interrupted out of.
    ///
    /// puku-cli reports an interrupted turn as
    /// `result subtype=error_during_execution is_error=true`, which is
    /// indistinguishable from a genuine failure at the line level. Without
    /// this, asking a session to stop marked it `failed` -- the user pressed
    /// stop and the platform called it an error.
    interrupted: Arc<AtomicBool>,
    /// Ships spilled event payloads to object storage.
    uploader: crate::uploader::Uploader,
    /// Where the runner writes those payloads on the session volume.
    blobs_dir: PathBuf,
}

pub struct SessionActor {
    pub spec: SessionSpec,
    pub state_dir: PathBuf,
    pub up_tx: mpsc::UnboundedSender<Up>,
    pub sessions: SessionMap,
    /// Override for the in-guest runner command (dev/testing; production
    /// always runs `puku-runner` from the image).
    pub runner_cmd: Option<String>,
    /// True when reattaching to a VM that survived a workerd restart.
    pub recovered: bool,
    /// Apply the multi-tenant isolation floor + egress policy (M3).
    pub multi_tenant: bool,
    pub egress_allow: Vec<String>,
    /// Hosts the model credential may be substituted into at the boundary.
    pub secret_hosts: Vec<String>,
    /// Opt-in network-boundary secret injection (see workerd main.rs for
    /// why plain env is the default).
    pub secret_env_injection: bool,
    /// Ships oversized event payloads to object storage.
    pub uploader: crate::uploader::Uploader,
    /// Mints the push token when a session finishes with work to push.
    pub git_tokens: crate::gitpush::GitTokens,
    /// The engine `spec.engine` names, as this worker runs it.
    pub backend: Arc<dyn VmBackend>,
    /// Where the session's files live (this host's disk, or an RBD image).
    pub volumes: crate::volumes::SessionVolumes,
}

/// Download, verify and unpack one skill pack.
///
/// The digest check is the whole security story here. A skill is text the
/// model will follow, so a swapped object in storage is an instruction
/// injection. The registry tells us the sha256 at resolve time over an
/// authenticated channel; if the bytes do not match, nothing is written.
async fn install_pack(
    pack: &puku_cloud_proto::session::SkillPackRef,
    root: &Path,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    let resp = reqwest::Client::new()
        .get(&pack.url)
        .send()
        .await
        .context("fetching the pack")?;
    if !resp.status().is_success() {
        anyhow::bail!("pack storage returned {}", resp.status());
    }
    let bytes = resp.bytes().await.context("reading the pack body")?;

    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != pack.digest {
        anyhow::bail!(
            "digest mismatch: expected {}, got {actual} — refusing to install",
            pack.digest
        );
    }

    // Unpack with tar rather than in-process: it is already present, and
    // streaming a pack through the worker's heap buys nothing. The registry
    // has already rejected absolute paths, `..` and links at publish time,
    // and the digest check above proves these are those same bytes.
    let staged = root.join(format!(".{}-{}.tgz", pack.name, pack.version));
    std::fs::write(&staged, &bytes)?;
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&staged)
        .arg("-C")
        .arg(root)
        .status()
        .await
        .context("running tar")?;
    let _ = std::fs::remove_file(&staged);
    if !status.success() {
        anyhow::bail!("tar exited with {status}");
    }
    Ok(())
}

/// Stream a presigned URL to disk. Used for imported transcripts, which can
/// be tens of megabytes and should not be buffered whole.
async fn fetch_to_file(url: &str, dest: &Path) -> Result<usize> {
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .context("fetching the object")?;
    if !resp.status().is_success() {
        anyhow::bail!("object storage returned {}", resp.status());
    }
    let bytes = resp.bytes().await.context("reading the object body")?;
    std::fs::write(dest, &bytes).context("writing the object to the session volume")?;
    Ok(bytes.len())
}

/// The uid the guest images run their agent as (`agent` in
/// images/puku-agent/Dockerfile). Session volumes are handed to it so the
/// guest can write to them whatever the engine's file sharing does with
/// ownership.
pub const GUEST_UID: u32 = 1000;

/// Give a directory the workerd created to the guest's uid. Best effort: a
/// worker not running as root (dev on a laptop) cannot chown, and the
/// directories are already its own there, so failing quietly is right.
fn hand_to_guest(path: &Path) {
    #[cfg(unix)]
    if let Err(e) = std::os::unix::fs::chown(path, Some(GUEST_UID), Some(GUEST_UID)) {
        tracing::debug!(path = %path.display(), error = %e, "chown to the guest uid skipped");
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub use crate::volumes::session_base_dir;

impl SessionActor {
    fn send_state(&self, state: SessionState, error: Option<String>) {
        let _ = self.up_tx.send(Up::SessionState {
            session_id: self.spec.session_id,
            state,
            error,
            puku_session_id: None,
        });
    }

    pub async fn run(self, input_rx: mpsc::UnboundedReceiver<InputCmd>) {
        let session_id = self.spec.session_id;
        let (volumes, state_dir) = (self.volumes.clone(), self.state_dir.clone());
        match self.run_inner(input_rx).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(%session_id, error = format!("{e:#}"), "session actor failed");
                // An error path that skipped the normal teardown must not
                // leave the disk mapped here, where no other host can open it.
                if let Err(e) = volumes.close(&state_dir, session_id).await {
                    tracing::error!(%session_id, error = format!("{e:#}"), "releasing the session disk failed");
                }
            }
        }
    }

    async fn run_inner(self, mut input_rx: mpsc::UnboundedReceiver<InputCmd>) -> Result<()> {
        let session_id = self.spec.session_id;
        let base = session_base_dir(&self.state_dir, session_id);
        let data = match self.volumes.open(&self.state_dir, session_id).await {
            Ok(d) => d,
            Err(e) => {
                self.finish_failed(&base, format!("{e:#}"));
                return Ok(());
            }
        };
        let session_dir = data.join("session");
        let workspace_dir = data.join("workspace");
        std::fs::create_dir_all(&session_dir)?;
        std::fs::create_dir_all(&workspace_dir)?;
        // The guest's agent runs as uid 1000 and these were created root-owned
        // 0755, so anything it wrote at the top of /workspace -- PUKU.md for
        // the memory preamble, first of all -- failed with EACCES
        // (runner.mjs records it). The top-level directories only: files
        // workerd plants below them (session.json at 0600) keep their own
        // ownership on purpose.
        hand_to_guest(&session_dir);
        hand_to_guest(&workspace_dir);

        if !self.recovered {
            // Bootstrap manifest: per-session data the runner reads at boot.
            let mut manifest = GuestManifest::from(&self.spec);
            // The guest serves this from its egress proxy so a blocked host
            // reads as a policy denial rather than a DNS failure.
            manifest.egress_allow = self.egress_allow.clone();
            std::fs::write(
                session_dir.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            // Persist the spec so a restarted workerd can reconcile.
            std::fs::write(base.join("spec.json"), serde_json::to_vec(&self.spec)?)?;

            // Materialize skill packs into the guest's skills root before
            // the agent starts. puku-cli discovers ~/.puku-cli/skills with
            // no configuration, so nothing else has to know about this.
            if !self.spec.skills.is_empty() {
                let root = session_dir.join("home/.puku-cli/skills");
                std::fs::create_dir_all(&root)?;
                for p in &self.spec.skills {
                    if let Err(e) = install_pack(p, &root).await {
                        // Running without a pack the session was told to
                        // use produces an agent that looks incompetent
                        // rather than mis-configured. Fail loudly instead.
                        self.finish_failed(
                            &base,
                            format!("installing skill pack {}@{}: {e:#}", p.name, p.version),
                        );
                        return Err(e);
                    }
                }
                tracing::info!(
                    %session_id,
                    packs = self.spec.skills.len(),
                    "installed skill packs"
                );
            }

            // Plant an imported transcript. The runner moves it into the
            // project directory once it knows its own cwd — the directory
            // name is derived from the working directory, and only the
            // guest knows whether it ended up in /workspace or
            // /workspace/repo.
            if let Some(import) = &self.spec.import {
                let dir = session_dir.join("import");
                std::fs::create_dir_all(&dir)?;
                let target = dir.join("transcript.jsonl");
                match import {
                    puku_cloud_proto::session::ImportRef::Inline { jsonl } => {
                        std::fs::write(&target, jsonl)?;
                    }
                    puku_cloud_proto::session::ImportRef::Url { url } => {
                        match fetch_to_file(url, &target).await {
                            Ok(bytes) => tracing::info!(%session_id, bytes, "imported transcript"),
                            Err(e) => {
                                // Continuing without it would start a
                                // "resumed" session with no history, which
                                // reads to the user as amnesia. Fail loudly.
                                self.finish_failed(
                                    &base,
                                    format!("downloading the imported transcript failed: {e:#}"),
                                );
                                return Err(e);
                            }
                        }
                    }
                }
            }

            // Plant the puku session credential in the guest HOME. Never
            // overwrite one already on the volume (a resumed session may
            // hold refreshed tokens newer than controld's copy).
            if let Some(sj) = &self.spec.puku_session_json {
                let cfg_dir = session_dir.join("home/.config/pukucode");
                let target = cfg_dir.join("session.json");
                if !target.exists() {
                    std::fs::create_dir_all(&cfg_dir)?;
                    std::fs::write(&target, sj)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
                    }
                }
            }
        }

        let sandbox = if self.recovered {
            match self.backend.attach(&self.spec.sandbox_name).await {
                Ok(vm) => vm,
                Err(e) => {
                    // The backend's context says which half failed: the VM
                    // is gone, or it is there and will not answer.
                    self.finish_failed(&base, format!("{e:#}"));
                    return Err(e);
                }
            }
        } else {
            self.send_state(SessionState::Booting, None);
            match self.create_sandbox(&session_dir, &workspace_dir).await {
                Ok(sb) => sb,
                Err(e) => {
                    self.finish_failed(&base, format!("boot failed: {e:#}"));
                    return Err(e);
                }
            }
        };

        if !self.recovered {
            self.send_state(SessionState::Bootstrapping, None);
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let (exit_tx, mut exit_rx) = mpsc::unbounded_channel();
        let agent_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let interrupted = Arc::new(AtomicBool::new(false));
        let activity = Arc::new(Mutex::new(Instant::now()));
        let pending_question = Arc::new(AtomicBool::new(false));
        let signals = TailSignals {
            exit_tx,
            agent_error: agent_error.clone(),
            interrupted: interrupted.clone(),
            activity: activity.clone(),
            pending_question: pending_question.clone(),
            uploader: self.uploader.clone(),
            blobs_dir: session_dir.join("blobs"),
        };
        // A recovered actor re-tails from 0: controld drops already-persisted
        // lines by guest_line, so redelivery is harmless.
        let cursor = if self.recovered { 0 } else { self.spec.events_cursor };
        let tail_task = tokio::spawn(tail_events(
            session_dir.join("events.ndjson"),
            cursor,
            session_id,
            self.up_tx.clone(),
            stop_rx,
            signals,
        ));

        let outcome = match self
            .drive(
                sandbox.as_ref(),
                &session_dir,
                &mut input_rx,
                &mut exit_rx,
                activity,
                pending_question,
                interrupted.clone(),
            )
            .await
        {
            Ok(o) => o,
            Err(e) => Outcome::Error(format!("{e:#}")),
        };

        // Let the tailer drain whatever the runner flushed last.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let _ = stop_tx.send(true);
        let _ = tail_task.await;

        // Tear the VM down; host volume dirs survive for resume/archive.
        if let Err(e) = sandbox.stop().await {
            tracing::warn!(
                %session_id, sandbox = %sandbox.name(), engine = %self.spec.engine,
                error = format!("{e:#}"), "sandbox stop failed"
            );
        }
        crate::vm::remove_with_retry(self.backend.as_ref(), &self.spec.sandbox_name, session_id)
            .await;

        let (state, error) = match outcome {
            Outcome::Exited(0) => (SessionState::Completed, None),
            Outcome::Exited(code) => {
                let msg = agent_error
                    .lock()
                    .ok()
                    .and_then(|m| m.clone())
                    // Nothing from the agent means the runner itself failed,
                    // and it wrote the reason to its stderr on the way out --
                    // a failed clone, a missing binary. Without this the
                    // operator gets "runner exited with code 70" and has to
                    // ssh to the box to learn it was
                    // "URL rejected: Malformed input to a URL function".
                    .or_else(|| runner_stderr_tail(&session_dir))
                    .unwrap_or_else(|| format!("runner exited with code {code}"));
                (SessionState::Failed, Some(msg))
            }
            Outcome::Parked => (SessionState::Stopped, None),
            Outcome::Crashed(ref why) => (SessionState::Stopped, Some(why.clone())),
            Outcome::Killed => (SessionState::Canceled, None),
            Outcome::Error(ref e) => (SessionState::Failed, Some(e.clone())),
        };
        // Get the work out before anything reaps the volume. Only for a
        // session that actually finished: a parked session will resume and
        // keep working, and pushing a half-done branch on every idle
        // timeout would spam the repo.
        if matches!(state, SessionState::Completed | SessionState::Failed) {
            if let Some(repo_url) = &self.spec.repo {
                crate::gitpush::push_session_branch(
                    session_id,
                    &workspace_dir,
                    repo_url,
                    &self.git_tokens,
                    &self.up_tx,
                )
                .await;
            }
        }

        // A parked session keeps spec.json volumes for resume, but the spec
        // no longer matches a live sandbox; drop it so reconcile skips it.
        let _ = std::fs::remove_file(base.join("spec.json"));
        // Hand the disk back so whichever host runs the next turn can open
        // it. Before reporting the state: controld may place a resume the
        // moment it hears the session stopped.
        if let Err(e) = self.volumes.close(&self.state_dir, session_id).await {
            tracing::error!(%session_id, error = format!("{e:#}"), "releasing the session disk failed");
        }
        match outcome {
            // Not a state the session chose: say what happened and let
            // controld decide whether to start it again.
            Outcome::Crashed(detail) => {
                let _ = self.up_tx.send(Up::SessionCrashed { session_id, detail });
            }
            _ => self.send_state(state, error),
        }
        self.sessions.remove(session_id);
        Ok(())
    }

    fn finish_failed(&self, base: &Path, msg: String) {
        let _ = std::fs::remove_file(base.join("spec.json"));
        // A boot that failed part-way still registered a sandbox with msb,
        // and the normal teardown never runs for it. Those accumulate: two
        // on the first deployment, the older one pinning a retired image,
        // reported as drift by /v1/fleet with no way to act on it short of
        // an ssh and `msb rm`.
        let name = self.spec.sandbox_name.clone();
        let id = self.spec.session_id;
        let backend = self.backend.clone();
        let volumes = self.volumes.clone();
        let state_dir = self.state_dir.clone();
        tokio::spawn(async move {
            crate::vm::remove_with_retry(backend.as_ref(), &name, id).await;
            if let Err(e) = volumes.close(&state_dir, id).await {
                tracing::error!(session_id = %id, error = format!("{e:#}"), "releasing the session disk failed");
            }
        });
        self.send_state(SessionState::Failed, Some(msg));
        self.sessions.remove(self.spec.session_id);
    }

    async fn create_sandbox(&self, session_dir: &Path, workspace_dir: &Path) -> Result<Box<dyn Vm>> {
        let vm_spec = self.vm_spec(session_dir, workspace_dir);
        self.backend.create(&vm_spec).await.context("creating sandbox")
    }

    /// The engine-neutral description of this session's VM.
    fn vm_spec(&self, session_dir: &Path, workspace_dir: &Path) -> VmSpec {
        let spec = &self.spec;
        let mut vm = VmSpec {
            name: spec.sandbox_name.clone(),
            image: spec.image.clone(),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            mounts: vec![
                Mount { guest: "/session".into(), host: session_dir.to_path_buf() },
                Mount { guest: "/workspace".into(), host: workspace_dir.to_path_buf() },
            ],
            labels: vec![
                ("puku.session".into(), spec.session_id.to_string()),
                ("puku.managed".into(), "true".into()),
            ],
            max_duration_s: Some(spec.max_duration_s as u64),
            env: Vec::new(),
            secrets: Vec::new(),
            egress_allow: self.egress_allow.clone(),
            multi_tenant: self.multi_tenant,
            ports: Vec::new(),
            root_disk: None,
            shm_mib: None,
        };

        // Model auth: a puku platform API key (sent by puku-cli as the
        // x-api-key header) or a puku subscription token (Bearer). Both go
        // to PUKU_AI_BASE_URL — api-cli.puku.sh by default. Delivered as
        // plain env by default: msb's boundary injection MITMs guest TLS
        // and puku-cli's networking stalls under that proxy (empirically:
        // silent exit 0 after 30s phone-home timeouts). Opt back in with
        // --secret-env-injection once the two are compatible.
        // Route the guest at the model gateway, matching what puku-cowork's
        // spawnerd does for local VM sessions (spawn.go: ANTHROPIC_AUTH_TOKEN
        // + ANTHROPIC_BASE_URL; index.ts adds PUKU_AI_BASE_URL /
        // PUKU_WORKER_URL / PUKU_API_KEY). Same contract on both sides means
        // one credential convention in the ecosystem instead of two.
        if let Some(base) = &spec.puku_api_base {
            for var in ["ANTHROPIC_BASE_URL", "PUKU_AI_BASE_URL", "PUKU_WORKER_URL"] {
                vm.env.push((var.into(), base.clone()));
            }
        }
        if spec.puku_auth_token.is_some() {
            // puku-cli's own browser-OAuth flow can't complete in a headless
            // VM; PUKU_AUTH=0 tells it to read the bearer from the env
            // instead of trying.
            vm.env.push(("PUKU_AUTH".into(), "0".into()));
        }

        for (var, value) in [
            ("PUKU_AI_API_KEY", &spec.puku_api_key),
            ("PUKU_CLI_OAUTH_TOKEN", &spec.puku_oauth_token),
            ("ANTHROPIC_AUTH_TOKEN", &spec.puku_auth_token),
            // Connector MCP headers are written as `Bearer ${PUKU_API_KEY}`
            // and expanded from the env by puku-cli.
            ("PUKU_API_KEY", &spec.puku_auth_token),
        ] {
            if let Some(v) = clean_secret(value.as_deref()) {
                if self.secret_env_injection {
                    vm.secrets.push(SecretEnv {
                        var: var.into(),
                        value: v.to_string(),
                        hosts: self.secret_hosts.clone(),
                    });
                } else {
                    vm.env.push((var.into(), v.to_string()));
                }
            }
        }
        vm
    }

    /// The command that actually starts the agent in the guest.
    ///
    /// Resolved in one place so `launch_runner` and `uses_sdk_runner` cannot
    /// disagree about which runner is in there.
    fn runner_command(&self) -> String {
        self.runner_cmd
            .clone()
            .unwrap_or_else(|| DEFAULT_RUNNER_CMD.to_string())
    }

    /// Whether the guest is running the SDK runner rather than the bash one.
    ///
    /// This used to be `self.runner_cmd.is_some()`, which was only ever
    /// correct while the SDK runner was *exclusively* an override. The moment
    /// it became the default, `runner_cmd` was `None` for every production
    /// session and the interrupt below took the bash branch -- sending a raw
    /// `control_request` down a fifo that `runner.mjs` does not intercept, so
    /// it would be handed to the SDK as if it were a user message. The
    /// interrupt would not interrupt, and a malformed turn would be injected.
    ///
    /// Keyed on the resolved command, so an operator who overrides back to
    /// the bash runner still gets the bash mechanism.
    fn uses_sdk_runner(&self) -> bool {
        self.runner_command().contains("runner.mjs")
    }

    /// Launch the runner detached from any exec client: a short exec
    /// daemonizes it with setsid, so it belongs to the VM and survives
    /// workerd restarts. The launch wrapper appends the `exec.exited`
    /// marker to the outbox — the only exit signal the actor needs.
    async fn launch_runner(&self, sandbox: &dyn Vm, session_dir: &Path) -> Result<()> {
        std::fs::write(session_dir.join(".runner-cmd.sh"), self.runner_command())?;
        // printf, not echo: dash's echo mangles backslash escapes, and the
        // exit marker must be valid JSON under any /bin/sh.
        std::fs::write(
            session_dir.join("runner-launch.sh"),
            "#!/bin/sh\nsh /session/.runner-cmd.sh >> /session/runner.stderr 2>&1\ncode=$?\n\
             printf '{\"type\":\"exec.exited\",\"code\":%s}\\n' \"$code\" >> /session/events.ndjson\n",
        )?;
        let out = sandbox
            .exec(ExecRequest::sh(
                "setsid sh /session/runner-launch.sh < /dev/null > /dev/null 2>&1 &",
            ))
            .await
            .context("launching puku-runner")?;
        if !out.success() {
            anyhow::bail!("runner launch exec failed: {}", out.code);
        }
        Ok(())
    }

    /// Pump input commands and watch for the runner's outbox exit marker.
    ///
    /// Eight parameters against clippy's seven. Each is a distinct capability
    /// this loop needs -- the sandbox, the session directory, the streams, the
    /// interrupt flag -- and bundling them into a struct that exists only to
    /// satisfy a lint would move the arguments, not reduce them.
    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        sandbox: &dyn Vm,
        session_dir: &Path,
        input_rx: &mut mpsc::UnboundedReceiver<InputCmd>,
        exit_rx: &mut mpsc::UnboundedReceiver<i32>,
        activity: Arc<Mutex<Instant>>,
        pending_question: Arc<AtomicBool>,
        interrupted: Arc<AtomicBool>,
    ) -> Result<Outcome> {
        if !self.recovered {
            self.launch_runner(sandbox, session_dir).await?;
        }
        self.send_state(SessionState::Running, None);

        let idle_timeout = Duration::from_secs(self.spec.idle_timeout_s.max(60) as u64);
        let mut idle_tick = tokio::time::interval(Duration::from_secs(15));
        let mut watchdog_tick = tokio::time::interval(crate::watchdog::PROBE_EVERY);
        watchdog_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut watchdog = crate::watchdog::Watchdog::default();

        loop {
            tokio::select! {
                cmd = input_rx.recv() => match cmd {
                    Some(InputCmd::Line(line)) => {
                        pending_question.store(false, Ordering::Relaxed);
                        *activity.lock().unwrap() = Instant::now();
                        if let Err(e) = deliver_line(sandbox, &line).await {
                            tracing::warn!(error = %e, "input delivery failed");
                        }
                    }
                    Some(InputCmd::Interrupt) => {
                        interrupted.store(true, Ordering::Relaxed);
                        // Which mechanism is right depends on which runner is
                        // in the guest, and the host is the only place that
                        // knows -- it chose. Asking the guest was tried and
                        // does not work: `pgrep -f puku-runner` matches the
                        // very shell running the pgrep, so the guard was
                        // always true.
                        //
                        // The distinction matters. Under the SDK runner the
                        // supervisor owns the puku-cli child, so SIGINT-ing
                        // that process tears the transport down and the
                        // session dies -- observed as
                        // "runner exited with code 1" after an interrupt.
                        if self.uses_sdk_runner() {
                            // The SDK runner owns the control channel, so it
                            // takes the ask on its own line and turns it into
                            // WarmQuery.interrupt().
                            let _ =
                                deliver_line(sandbox, "{\"type\":\"platform.interrupt\"}").await;
                        } else {
                            // The bash runner pipes the fifo straight to
                            // puku-cli's stdin, so send what puku-cli itself
                            // understands -- the same control_request the SDK
                            // sends. SIGINT was the old answer and it killed
                            // the session outright: the signal reached the
                            // runner as well as the CLI, so an interrupt
                            // ended the run with code 130 instead of ending
                            // the turn.
                            let frame = serde_json::json!({
                                "type": "control_request",
                                "request_id": uuid::Uuid::new_v4().to_string(),
                                "request": {"subtype": "interrupt"},
                            })
                            .to_string();
                            let _ = deliver_line(sandbox, &frame).await;
                        }
                    }
                    Some(InputCmd::Park) => return Ok(Outcome::Parked),
                    Some(InputCmd::Kill) => return Ok(Outcome::Killed),
                    None => return Ok(Outcome::Error("worker shutting down".into())),
                },
                code = exit_rx.recv() => {
                    // Exit marker seen in the outbox — the authoritative
                    // (and only) completion signal.
                    return Ok(Outcome::Exited(code.unwrap_or(-1)));
                }
                _ = watchdog_tick.tick() => {
                    if let Some(why) = watchdog.observe(crate::watchdog::probe(sandbox).await) {
                        tracing::error!(session = %self.spec.session_id, "{why}");
                        return Ok(Outcome::Crashed(why));
                    }
                }
                _ = idle_tick.tick() => {
                    // A session blocked on a question idles at 4x the normal
                    // timeout before parking (the answer may just be slow).
                    let factor = if pending_question.load(Ordering::Relaxed) { 4 } else { 1 };
                    if activity.lock().unwrap().elapsed() > idle_timeout * factor {
                        tracing::info!(session = %self.spec.session_id, "idle timeout — parking");
                        return Ok(Outcome::Parked);
                    }
                }
            }
        }
    }
}

/// Append one stream-json line to the guest stdin fifo via a short exec.
/// (A host-side write to a fifo on the shared mount is not portable across
/// virtio-fs, so the write happens inside the guest.)
async fn deliver_line(sandbox: &dyn Vm, line: &str) -> Result<()> {
    sandbox
        .exec(ExecRequest::sh("cat >> /session/stdin.fifo").with_stdin(format!("{line}\n")))
        .await?;
    Ok(())
}

/// Poll-tail the guest event outbox and forward new lines as batched
/// SessionEvents frames. `cursor` lines are skipped (already persisted).
async fn tail_events(
    path: PathBuf,
    mut cursor: i64,
    session_id: uuid::Uuid,
    up_tx: mpsc::UnboundedSender<Up>,
    mut stop_rx: watch::Receiver<bool>,
    signals: TailSignals,
) {
    let mut offset: u64 = 0;
    let mut line_no: i64 = 0;
    let mut partial = String::new();
    let mut sent_psid = false;

    loop {
        let stopping = *stop_rx.borrow();

        if let Ok(mut f) = tokio::fs::File::open(&path).await {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            if f.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                let mut buf = String::new();
                if f.read_to_string(&mut buf).await.is_ok() && !buf.is_empty() {
                    offset += buf.len() as u64;
                    partial.push_str(&buf);
                    let mut batch: Vec<GuestEvent> = Vec::new();
                    while let Some(nl) = partial.find('\n') {
                        let line: String = partial.drain(..=nl).collect();
                        let line = line.trim_end();
                        line_no += 1;
                        if line.is_empty() || line_no <= cursor {
                            continue;
                        }
                        cursor = line_no;
                        *signals.activity.lock().unwrap() = Instant::now();
                        let payload = serde_json::from_str::<serde_json::Value>(line)
                            .unwrap_or_else(|_| serde_json::json!({"type": "raw", "text": line}));

                        inspect_line(&payload, session_id, &up_tx, &mut sent_psid, &signals);

                        let ty = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let kind = if ty.starts_with("exec.") {
                            EventKind::Exec
                        } else {
                            EventKind::Agent
                        };
                        // A truncated line's full payload lives in a file on
                        // the session volume, which gets reaped. Ship it to
                        // object storage and record the durable key. The key
                        // is derived from (session, line), so the event can
                        // go out now and the upload can finish later.
                        let blob_ref = if ty == "truncated" {
                            match payload.get("blob").and_then(|b| b.as_str()) {
                                Some(name) => {
                                    let key = crate::uploader::blob_key(session_id, line_no);
                                    signals.uploader.upload(
                                        session_id,
                                        key.clone(),
                                        signals.blobs_dir.join(name),
                                        "application/json",
                                    );
                                    Some(key)
                                }
                                None => None,
                            }
                        } else {
                            None
                        };
                        batch.push(GuestEvent {
                            line: line_no,
                            ts: Utc::now(),
                            kind,
                            payload,
                            blob_ref,
                        });
                        if batch.len() >= MAX_EVENTS_PER_FRAME {
                            let _ = up_tx.send(Up::SessionEvents {
                                session_id,
                                events: std::mem::take(&mut batch),
                            });
                        }
                    }
                    if !batch.is_empty() {
                        let _ = up_tx.send(Up::SessionEvents { session_id, events: batch });
                    }
                }
            }
        }

        if stopping {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = stop_rx.changed() => {}
        }
    }
}

/// Trim a credential before it becomes a guest environment variable.
///
/// Guest env is carried on the kernel command line, which rejects control
/// bytes. A credential pasted into a `.env` file or a database row picks up
/// a trailing newline constantly, and untrimmed that surfaces as a VM
/// *build* failure naming the variable — technically accurate and useless,
/// because the value looks correct everywhere the operator can inspect it.
///
/// Surrounding whitespace on a secret is always a paste artifact, never
/// meaningful, so trimming is safe. Anything still unprintable after that is
/// left alone: a mangled credential should fail loudly rather than be
/// silently reshaped into a different one.
fn clean_secret(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Usage numbers carried by a puku-cli `result` event.
#[derive(Debug, PartialEq)]
struct ResultUsage {
    cost_usd: f64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
}

/// Cache reads/writes are billed differently from fresh input and are most
/// of the traffic in a long session, so they're counted apart rather than
/// folded into `tokens_in`.
fn parse_result_usage(payload: &serde_json::Value) -> ResultUsage {
    let usage = payload.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(|t| t.as_i64()).unwrap_or(0);

    let mut out = ResultUsage {
        cost_usd: payload.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0),
        tokens_in: count("input_tokens"),
        tokens_out: count("output_tokens"),
        cache_read_tokens: count("cache_read_input_tokens"),
        cache_write_tokens: count("cache_creation_input_tokens"),
    };

    // `usage` comes back zeroed against the puku gateway while `modelUsage`
    // carries the real figures -- observed on a live session billed at
    // $0.0957 with usage.input_tokens = 0 and modelUsage.inputTokens =
    // 16579. Cost was recorded and consumption was not, so every session on
    // the platform reported spend it could not account for.
    //
    // Only consulted when `usage` said nothing, so a CLI that fills it in
    // keeps winning and this stays a fallback rather than a second source
    // of truth.
    if out.tokens_in == 0 && out.tokens_out == 0 {
        if let Some(models) = payload.get("modelUsage").and_then(|m| m.as_object()) {
            let sum = |key: &str| -> i64 {
                models.values().filter_map(|m| m.get(key)).filter_map(|v| v.as_i64()).sum()
            };
            out.tokens_in = sum("inputTokens");
            out.tokens_out = sum("outputTokens");
            if out.cache_read_tokens == 0 {
                out.cache_read_tokens = sum("cacheReadInputTokens");
            }
            if out.cache_write_tokens == 0 {
                out.cache_write_tokens = sum("cacheCreationInputTokens");
            }
        }
    }
    out
}

/// Pull platform-relevant facts out of the agent stream: puku's own session
/// id (needed for --resume), the final result's cost/usage, blocked-on-user
/// questions, and the runner's exit marker.
fn inspect_line(
    payload: &serde_json::Value,
    session_id: uuid::Uuid,
    up_tx: &mpsc::UnboundedSender<Up>,
    sent_psid: &mut bool,
    signals: &TailSignals,
) {
    let ty = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if !*sent_psid {
        if let Some(psid) = payload.get("session_id").and_then(|s| s.as_str()) {
            *sent_psid = true;
            let _ = up_tx.send(Up::SessionState {
                session_id,
                state: SessionState::Running,
                error: None,
                puku_session_id: Some(psid.to_string()),
            });
        }
    }

    if ty == "exec.exited" {
        let code = payload.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
        let _ = signals.exit_tx.send(code);
    }

    if ty == "result" {
        // The turn is over. puku-cli runs with --input-format stream-json and
        // stays alive for a follow-up, so exec.exited never comes: without
        // this the session sits `running` until the idle timeout and then
        // reports `stopped`, having succeeded. Every session on this platform
        // did exactly that. Follow-up input resumes the session instead.
        let mut is_error = payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
        // An interrupt the platform asked for is a turn ending, not a
        // failure. Consumed here so a later, genuine error still fails.
        if is_error && signals.interrupted.swap(false, Ordering::Relaxed) {
            tracing::debug!(%session_id, "error result follows an interrupt; ending the turn");
            is_error = false;
        }
        if is_error {
            let msg = payload
                .get("result")
                .and_then(|v| v.as_str())
                .map(|m| m.chars().take(400).collect::<String>())
                .unwrap_or_else(|| "the agent ended its turn with an error".into());
            if let Ok(mut slot) = signals.agent_error.lock() {
                *slot = Some(msg);
            }
        }
        let u = parse_result_usage(payload);
        let _ = up_tx.send(Up::SessionUsage {
            session_id,
            cost_usd: u.cost_usd,
            tokens_in: u.tokens_in,
            tokens_out: u.tokens_out,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
        });
        // After the usage, so the final numbers are recorded before the
        // actor tears the sandbox down.
        let _ = signals.exit_tx.send(if is_error { 1 } else { 0 });
    }

    if let Some(question) = detect_question(payload) {
        signals.pending_question.store(true, Ordering::Relaxed);
        let _ = up_tx.send(Up::PendingQuestion { session_id, question });
    }
}

/// Does this agent event block on user input? Matches puku-cli's question
/// surfaces: AskUserQuestion / ExitPlanMode tool calls, and control_request
/// permission frames.
/// Verified against puku-cli 1.8.43 by driving a real headless session:
///
/// * `AskUserQuestion` (and any tool whose permission check says "ask")
///   emits `{"type":"control_request","request_id":…,"request":{
///   "subtype":"can_use_tool","tool_name":…,"input":…,"tool_use_id":…}}`.
///   Its `checkPermissions` returns `behavior:"ask"` unconditionally, so
///   this frame is the *only* reliable question signal.
/// * The matching assistant `tool_use` block arrives too, but answering it
///   is not possible — the CLI is blocked on the control channel, so
///   detecting the block would park the session on a question the platform
///   then couldn't clear.
/// * Every other `control_request` subtype (e.g. `interrupt` acks) is
///   platform bookkeeping, not a question. Parking on those — which the
///   previous unconditional match did — freezes a healthy session.
///
/// The CLI waits indefinitely for the reply (measured: 75 s with no
/// timeout), which is what makes a human-in-the-loop cloud session possible
/// at all.
fn detect_question(payload: &serde_json::Value) -> Option<serde_json::Value> {
    // The SDK runner answers the CLI itself through `canUseTool`, so the
    // control_request never reaches the outbox. It republishes the ask as
    // `platform.question` instead — same projection, so everything
    // downstream (waiting_input, the dashboard, notifications) is unchanged.
    //
    // `kind` records which dialect asked, because the answer has to go back
    // in the same one: a control_response envelope for the CLI, a
    // platform.answer line for the runner.
    if payload.get("type").and_then(|t| t.as_str()) == Some("platform.question") {
        let request_id = payload.get("request_id").and_then(|r| r.as_str())?;
        return Some(serde_json::json!({
            "kind": "platform.question",
            "request_id": request_id,
            "tool_name": payload.get("tool_name"),
            "tool_use_id": payload.get("tool_use_id"),
            "input": payload.get("input"),
        }));
    }
    if payload.get("type").and_then(|t| t.as_str()) != Some("control_request") {
        return None;
    }
    let request = payload.get("request")?;
    if request.get("subtype").and_then(|s| s.as_str()) != Some("can_use_tool") {
        return None;
    }
    // request_id is what the control_response must echo; without it the
    // question is unanswerable, so don't park the session on it.
    let request_id = payload.get("request_id").and_then(|r| r.as_str())?;
    Some(serde_json::json!({
        "kind": "can_use_tool",
        "request_id": request_id,
        "tool_name": request.get("tool_name"),
        "tool_use_id": request.get("tool_use_id"),
        "input": request.get("input"),
    }))
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    /// Fixture captured verbatim from a real `puku-cli -p --output-format
    /// stream-json` run, so the field names stay pinned to what puku-cli
    /// actually emits.
    const REAL_RESULT: &str = r#"{"type":"result","total_cost_usd":0.078139,
        "usage":{"input_tokens":15595,"output_tokens":4,
        "cache_read_input_tokens":128,"cache_creation_input_tokens":0}}"#;

    #[test]
    fn parses_real_puku_cli_result_event() {
        let payload: serde_json::Value = serde_json::from_str(REAL_RESULT).unwrap();
        assert_eq!(
            parse_result_usage(&payload),
            ResultUsage {
                cost_usd: 0.078139,
                tokens_in: 15595,
                tokens_out: 4,
                cache_read_tokens: 128,
                cache_write_tokens: 0,
            }
        );
    }

    /// A cache-heavy turn: the cached read dwarfs fresh input, which is the
    /// traffic that used to go unrecorded entirely.
    #[test]
    fn counts_cache_heavy_turn() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"type":"result","total_cost_usd":0.5,
                "usage":{"input_tokens":12,"output_tokens":300,
                "cache_read_input_tokens":184320,"cache_creation_input_tokens":4096}}"#,
        )
        .unwrap();
        let u = parse_result_usage(&payload);
        assert_eq!(u.cache_read_tokens, 184320);
        assert_eq!(u.cache_write_tokens, 4096);
        assert_eq!(u.tokens_in, 12);
    }

    #[test]
    fn missing_usage_object_is_zeroed_not_panicking() {
        let payload: serde_json::Value =
            serde_json::from_str(r#"{"type":"result","subtype":"error"}"#).unwrap();
        assert_eq!(
            parse_result_usage(&payload),
            ResultUsage {
                cost_usd: 0.0,
                tokens_in: 0,
                tokens_out: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }
        );
    }
}

#[cfg(test)]
mod vm_spec_tests {
    use super::*;

    fn actor(secret_env_injection: bool) -> SessionActor {
        let mut a = super::runner_kind_tests::actor_with(None);
        a.secret_env_injection = secret_env_injection;
        a.secret_hosts = vec!["api-cli.puku.sh".into()];
        a.egress_allow = vec!["puku.sh".into()];
        a.spec.puku_api_key = Some("key-value\n".into());
        a.spec.puku_api_base = Some("https://gw.test".into());
        a
    }

    /// The spec a backend receives must carry exactly what the msb builder
    /// was handed before the seam existed.
    #[test]
    fn the_spec_carries_mounts_labels_policy_and_env() {
        let a = actor(false);
        let vm = a.vm_spec(Path::new("/s/session"), Path::new("/s/workspace"));
        assert_eq!(vm.name, "ses-test");
        assert_eq!(vm.mounts[0], Mount { guest: "/session".into(), host: "/s/session".into() });
        assert_eq!(vm.mounts[1].guest, "/workspace");
        assert!(vm.labels.contains(&("puku.managed".into(), "true".into())));
        assert_eq!(vm.max_duration_s, Some(3600));
        assert_eq!(vm.egress_allow, vec!["puku.sh".to_string()]);
        assert!(vm.env.contains(&("PUKU_AI_BASE_URL".into(), "https://gw.test".into())));
        // Trimmed, as the kernel command line demands.
        assert!(vm.env.contains(&("PUKU_AI_API_KEY".into(), "key-value".into())));
        assert!(vm.secrets.is_empty());
    }

    /// With boundary injection on, the credential must not ride as env.
    #[test]
    fn boundary_injection_moves_credentials_out_of_env() {
        let a = actor(true);
        let vm = a.vm_spec(Path::new("/s"), Path::new("/w"));
        assert!(!vm.env.iter().any(|(k, _)| k == "PUKU_AI_API_KEY"));
        assert_eq!(vm.secrets.len(), 1);
        assert_eq!(vm.secrets[0].var, "PUKU_AI_API_KEY");
        assert_eq!(vm.secrets[0].hosts, vec!["api-cli.puku.sh".to_string()]);
    }

    /// Input reaches the guest as one exec that appends to the fifo.
    #[tokio::test]
    async fn a_line_is_delivered_through_the_fifo() {
        let backend = crate::vm::fake::FakeBackend::new(puku_cloud_proto::Engine::Libkrun);
        let vm = backend.create(&VmSpec { name: "ses-x".into(), ..Default::default() }).await.unwrap();
        assert_eq!(backend.created().len(), 1);
        deliver_line(vm.as_ref(), "{\"type\":\"user\"}").await.unwrap();
        let (_, req) = backend.execs().pop().unwrap();
        assert_eq!(req.args[1], "cat >> /session/stdin.fifo");
        assert_eq!(req.stdin.as_deref(), Some(&b"{\"type\":\"user\"}\n"[..]));
    }
}

#[cfg(test)]
mod question_tests {
    use super::detect_question;

    /// Captured verbatim from a real `puku-cli 1.8.43` headless run driven
    /// with `--permission-prompt-tool stdio`. This is the frame the whole
    /// waiting_input flow hangs off, so it stays pinned to reality.
    const REAL_ASK: &str = r#"{
        "type":"control_request",
        "request_id":"81cc6dd1-9d6d-4464-ba1d-d0965702df79",
        "request":{
            "subtype":"can_use_tool",
            "tool_name":"AskUserQuestion",
            "input":{"questions":[{
                "question":"Do you prefer tabs or spaces for code indentation?",
                "header":"Indentation",
                "options":[{"label":"Tabs","description":"Use tab characters"},
                           {"label":"Spaces","description":"Use space characters"}],
                "multiSelect":false}]},
            "tool_use_id":"call_f07f2467ac3447a0b369118f"}}"#;

    #[test]
    fn detects_a_real_can_use_tool_request() {
        let payload: serde_json::Value = serde_json::from_str(REAL_ASK).unwrap();
        let q = detect_question(&payload).expect("should park on a permission ask");
        assert_eq!(q["kind"], "can_use_tool");
        assert_eq!(q["request_id"], "81cc6dd1-9d6d-4464-ba1d-d0965702df79");
        assert_eq!(q["tool_name"], "AskUserQuestion");
        assert_eq!(q["tool_use_id"], "call_f07f2467ac3447a0b369118f");
        // The question body has to survive: it is what the human is shown.
        assert_eq!(q["input"]["questions"][0]["header"], "Indentation");
    }

    /// The bug this replaces: every control_request parked the session, so a
    /// routine interrupt ack froze a healthy run.
    #[test]
    fn ignores_control_requests_that_are_not_permission_asks() {
        let interrupt = serde_json::json!({
            "type": "control_request",
            "request_id": "abc",
            "request": {"subtype": "interrupt"},
        });
        assert!(detect_question(&interrupt).is_none());
    }

    /// An assistant tool_use block for AskUserQuestion arrives alongside the
    /// control_request. Parking on it too would raise a second, unanswerable
    /// question for the same ask.
    #[test]
    fn ignores_the_assistant_tool_use_block() {
        let assistant = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "call_f07f2467ac3447a0b369118f",
                "name": "AskUserQuestion",
                "input": {"questions": []},
            }]},
        });
        assert!(detect_question(&assistant).is_none());
    }

    /// No request_id means no way to build a control_response, so parking
    /// would strand the session.
    #[test]
    fn ignores_a_permission_ask_with_no_request_id() {
        let malformed = serde_json::json!({
            "type": "control_request",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash"},
        });
        assert!(detect_question(&malformed).is_none());
    }

    /// The SDK runner answers the CLI itself, so no control_request ever
    /// reaches the outbox; it republishes the ask in the platform's own
    /// shape. Same projection out, so waiting_input and the dashboard do not
    /// care which runner produced it.
    #[test]
    fn a_platform_question_is_a_question() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"type":"platform.question","request_id":"req-9",
                "tool_name":"AskUserQuestion","tool_use_id":"tu-9",
                "input":{"questions":[{"header":"Bucket"}]}}"#,
        )
        .unwrap();
        let q = detect_question(&payload).expect("should park the session");
        assert_eq!(q["kind"], "platform.question");
        assert_eq!(q["request_id"], "req-9");
        assert_eq!(q["tool_name"], "AskUserQuestion");
        assert_eq!(q["input"]["questions"][0]["header"], "Bucket");
    }

    /// `kind` is what tells controld which dialect to answer in. Losing it
    /// would send a control_response to a runner that cannot read one.
    #[test]
    fn the_two_dialects_are_distinguishable() {
        let cli: serde_json::Value = serde_json::from_str(REAL_ASK).unwrap();
        let sdk: serde_json::Value = serde_json::from_str(
            r#"{"type":"platform.question","request_id":"r","input":{}}"#,
        )
        .unwrap();
        assert_eq!(detect_question(&cli).unwrap()["kind"], "can_use_tool");
        assert_eq!(detect_question(&sdk).unwrap()["kind"], "platform.question");
    }

    /// Unanswerable without a correlator, so it must not park the session.
    #[test]
    fn a_platform_question_without_a_request_id_is_ignored() {
        let payload: serde_json::Value =
            serde_json::from_str(r#"{"type":"platform.question","input":{}}"#).unwrap();
        assert!(detect_question(&payload).is_none());
    }
}

#[cfg(test)]
mod secret_tests {
    use super::clean_secret;

    #[test]
    fn trims_the_trailing_newline_a_paste_leaves() {
        // The real failure: `echo 'KEY=...' >> .env` or a copied token both
        // arrive with \n, and msb rejects the kernel command line with
        // "cannot be carried ... contains control or non-ASCII bytes".
        assert_eq!(clean_secret(Some("pkc_abc123\n")), Some("pkc_abc123"));
        assert_eq!(clean_secret(Some("  pkc_abc123  ")), Some("pkc_abc123"));
        assert_eq!(clean_secret(Some("pkc_abc123\r\n")), Some("pkc_abc123"));
    }

    #[test]
    fn whitespace_only_is_the_same_as_absent() {
        // Otherwise an empty .env line sets the variable to "" in the guest,
        // and puku-cli treats a present-but-empty credential as configured.
        assert_eq!(clean_secret(Some("")), None);
        assert_eq!(clean_secret(Some("   \n")), None);
        assert_eq!(clean_secret(None), None);
    }

    #[test]
    fn leaves_a_clean_value_untouched() {
        assert_eq!(clean_secret(Some("pkc_abc123")), Some("pkc_abc123"));
    }

    #[test]
    fn does_not_reshape_an_interior_mangle() {
        // A control byte in the middle is not a paste artifact. Passing it
        // through means msb fails loudly and names the variable, which beats
        // silently sending a credential the operator never typed.
        assert_eq!(clean_secret(Some("pkc_ab\ncd")), Some("pkc_ab\ncd"));
    }
}

#[cfg(test)]
mod turn_completion_tests {
    use super::*;
    use uuid::Uuid;

    pub(super) fn signals() -> (TailSignals, mpsc::UnboundedReceiver<i32>, Arc<Mutex<Option<String>>>)
    {
        signals_with(Arc::new(AtomicBool::new(false)))
    }

    pub(super) fn signals_with(
        interrupted: Arc<AtomicBool>,
    ) -> (TailSignals, mpsc::UnboundedReceiver<i32>, Arc<Mutex<Option<String>>>) {
        let (exit_tx, exit_rx) = mpsc::unbounded_channel();
        let agent_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        (
            TailSignals {
                exit_tx,
                agent_error: agent_error.clone(),
                activity: Arc::new(Mutex::new(Instant::now())),
                pending_question: Arc::new(AtomicBool::new(false)),
                interrupted,
                uploader: {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    crate::uploader::Uploader::new(tx)
                },
                blobs_dir: std::path::PathBuf::from("/tmp"),
            },
            exit_rx,
            agent_error,
        )
    }

    /// The regression this exists for: puku-cli stays alive after a result,
    /// so exec.exited never arrives and a session that succeeded sat
    /// `running` until the idle timeout, then reported `stopped`.
    #[test]
    fn a_successful_result_ends_the_turn() {
        let (sig, mut rx, _) = signals();
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        let payload = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "total_cost_usd": 0.01,
        });
        inspect_line(&payload, Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert_eq!(rx.try_recv().ok(), Some(0), "a success must complete the session");
    }

    #[test]
    fn an_error_result_fails_with_the_agents_own_words() {
        let (sig, mut rx, err) = signals();
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        let payload = serde_json::json!({
            "type": "result", "subtype": "error_during_execution", "is_error": true,
            "result": "API Error: 429 quota_exceeded",
        });
        inspect_line(&payload, Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert_eq!(rx.try_recv().ok(), Some(1));
        assert_eq!(
            err.lock().unwrap().as_deref(),
            Some("API Error: 429 quota_exceeded"),
            "\"runner exited with code 1\" tells an operator nothing"
        );
    }

    /// A non-terminal line must not end the session.
    #[test]
    fn an_assistant_message_does_not_end_the_turn() {
        let (sig, mut rx, _) = signals();
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        let payload = serde_json::json!({"type": "assistant", "message": {"content": []}});
        inspect_line(&payload, Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert!(rx.try_recv().is_err());
    }
}

/// The sandbox a session's VM runs under.
///
/// Derived rather than looked up so a reaper can name a sandbox it has no
/// session row for -- which is exactly the case that leaks. Must match
/// `db::create_session`, which mints it the same way.
pub fn sandbox_name_for(session_id: uuid::Uuid) -> String {
    format!("ses-{}", &session_id.simple().to_string()[..12])
}

/// The tail of the runner's stderr, for a failure the agent never reported.
///
/// Bounded on both ends: only the last few lines, and only the last 8 KiB of
/// the file, because a chatty runner should not be able to push a megabyte
/// of log into a session's error column. Redaction already ran on the event
/// stream, not on this file, so lines that look like credentials are
/// dropped rather than surfaced.
fn runner_stderr_tail(session_dir: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 8 * 1024;
    const LINES: usize = 4;

    let path = session_dir.join("runner.stderr");
    let mut f = std::fs::File::open(&path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    f.seek(SeekFrom::Start(len.saturating_sub(WINDOW))).ok()?;
    let mut buf = String::new();
    f.take(WINDOW).read_to_string(&mut buf).ok();

    let looks_secret = |l: &str| {
        let l = l.to_ascii_lowercase();
        ["token", "api_key", "apikey", "secret", "password", "authorization", "bearer "]
            .iter()
            .any(|k| l.contains(k))
    };
    let tail: Vec<&str> = buf
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !looks_secret(l))
        .rev()
        .take(LINES)
        .collect();
    if tail.is_empty() {
        return None;
    }
    let msg: String =
        tail.into_iter().rev().collect::<Vec<_>>().join(" | ").chars().take(600).collect();
    Some(msg)
}

#[cfg(test)]
mod runner_stderr_tests {
    use super::runner_stderr_tail;

    /// A scratch dir under the test's own name, so the cases cannot
    /// clobber each other when the suite runs in parallel.
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    impl Scratch {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    fn dir_with(contents: &str) -> Scratch {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("puku-stderr-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("runner.stderr"), contents).unwrap();
        Scratch(d)
    }

    /// The failure this exists for: a clone that died with the reason only
    /// ever written to the box's disk.
    #[test]
    fn a_failed_clone_explains_itself() {
        let d = dir_with(
            "Cloning into '/workspace/repo'...\n\
             fatal: unable to access 'https://github.com/o/r/': URL rejected: Malformed input\n\
             puku-runner: git clone failed\n",
        );
        let msg = runner_stderr_tail(d.path()).expect("a non-empty stderr must yield a reason");
        assert!(msg.contains("URL rejected"), "got: {msg}");
        assert!(msg.contains("git clone failed"), "got: {msg}");
    }

    /// This text lands in an API field an operator reads, so a runner that
    /// echoes a credential must not have it copied there.
    #[test]
    fn credential_shaped_lines_are_dropped() {
        let d = dir_with("using ANTHROPIC_AUTH_TOKEN=sk-ant-secret\nboot failed\n");
        let msg = runner_stderr_tail(d.path()).unwrap();
        assert!(!msg.contains("sk-ant-secret"), "leaked a credential: {msg}");
        assert_eq!(msg, "boot failed");
    }

    #[test]
    fn an_empty_or_missing_stderr_yields_nothing() {
        assert_eq!(runner_stderr_tail(dir_with("").path()), None);
        assert_eq!(runner_stderr_tail(std::path::Path::new("/nonexistent")), None);
    }

    /// A chatty runner must not be able to push a megabyte into the
    /// session's error column.
    #[test]
    fn the_tail_is_bounded() {
        let big = "noise line\n".repeat(50_000) + "the actual failure\n";
        let msg = runner_stderr_tail(dir_with(&big).path()).unwrap();
        assert!(msg.len() <= 600, "unbounded: {} chars", msg.len());
        assert!(msg.ends_with("the actual failure"), "the last line must survive: {msg}");
    }
}

#[cfg(test)]
mod sandbox_name_tests {
    use super::sandbox_name_for;
    use uuid::Uuid;

    /// The reaper derives this name instead of reading it off a session
    /// row, so it has to match what controld minted at create time. A drift
    /// here means the reaper deletes nothing and says it succeeded.
    #[test]
    fn matches_the_name_controld_mints() {
        let id = Uuid::parse_str("43ffa9ff-b166-43bb-8506-e5f3d54c2846").unwrap();
        assert_eq!(sandbox_name_for(id), "ses-43ffa9ffb166");
        let id = Uuid::parse_str("d816ec91-366c-436c-b1f9-b6658ea0676d").unwrap();
        assert_eq!(sandbox_name_for(id), "ses-d816ec91366c");
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::*;
    use uuid::Uuid;

    fn error_result() -> serde_json::Value {
        // What puku-cli actually emits for an interrupted turn -- observed
        // on a live session, and identical in shape to a real failure.
        serde_json::json!({
            "type": "result", "subtype": "error_during_execution", "is_error": true,
        })
    }

    /// Pressing stop must not be reported as a failure.
    #[test]
    fn an_interrupted_turn_completes_rather_than_failing() {
        let flag = Arc::new(AtomicBool::new(true)); // the platform asked to stop
        let (sig, mut rx, err) = super::turn_completion_tests::signals_with(flag.clone());
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        inspect_line(&error_result(), Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert_eq!(rx.try_recv().ok(), Some(0), "an interrupt ends the turn cleanly");
        assert!(err.lock().unwrap().is_none(), "and records no error");
        assert!(!flag.load(Ordering::Relaxed), "the flag is consumed");
    }

    /// ...but the very next genuine error still fails, or one interrupt
    /// would mask every later failure in the session.
    #[test]
    fn a_later_genuine_error_still_fails() {
        let flag = Arc::new(AtomicBool::new(true));
        let (sig, mut rx, _) = super::turn_completion_tests::signals_with(flag.clone());
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        inspect_line(&error_result(), Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert_eq!(rx.try_recv().ok(), Some(0));

        let payload = serde_json::json!({
            "type": "result", "is_error": true, "result": "API Error: 500",
        });
        inspect_line(&payload, Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert_eq!(rx.try_recv().ok(), Some(1), "a real error after an interrupt must fail");
    }

    /// An error with no interrupt behind it is untouched.
    #[test]
    fn an_uninterrupted_error_fails_as_before() {
        let (sig, mut rx, err) = super::turn_completion_tests::signals();
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        let payload = serde_json::json!({
            "type": "result", "is_error": true, "result": "API Error: 429 quota_exceeded",
        });
        inspect_line(&payload, Uuid::new_v4(), &up_tx, &mut false, &sig);
        assert_eq!(rx.try_recv().ok(), Some(1));
        assert!(err.lock().unwrap().as_deref().unwrap().contains("429"));
    }
}

#[cfg(test)]
mod usage_fallback_tests {
    use super::parse_result_usage;

    /// The exact shape a live session returned: billed $0.0957, and every
    /// counter in `usage` zero.
    #[test]
    fn model_usage_covers_a_zeroed_usage_block() {
        let payload = serde_json::json!({
            "type": "result", "total_cost_usd": 0.095688,
            "usage": {
                "input_tokens": 0, "output_tokens": 0,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0
            },
            "modelUsage": {"puku-ai-2.8": {
                "inputTokens": 16579, "outputTokens": 173,
                "cacheReadInputTokens": 16936, "cacheCreationInputTokens": 0,
                "costUSD": 0.095688
            }}
        });
        let u = parse_result_usage(&payload);
        assert_eq!(u.tokens_in, 16579, "spend without consumption is unaccountable");
        assert_eq!(u.tokens_out, 173);
        assert_eq!(u.cache_read_tokens, 16936);
        assert!((u.cost_usd - 0.095688).abs() < 1e-9);
    }

    /// A populated `usage` stays authoritative, so this is a fallback and
    /// not a second source of truth.
    #[test]
    fn a_populated_usage_block_still_wins() {
        let payload = serde_json::json!({
            "type": "result", "total_cost_usd": 0.5,
            "usage": {"input_tokens": 11, "output_tokens": 22,
                      "cache_read_input_tokens": 33, "cache_creation_input_tokens": 44},
            "modelUsage": {"m": {"inputTokens": 999, "outputTokens": 999}}
        });
        let u = parse_result_usage(&payload);
        assert_eq!((u.tokens_in, u.tokens_out), (11, 22));
        assert_eq!((u.cache_read_tokens, u.cache_write_tokens), (33, 44));
    }

    /// A turn that used several models bills for all of them.
    #[test]
    fn multiple_models_are_summed() {
        let payload = serde_json::json!({
            "type": "result", "total_cost_usd": 1.0,
            "usage": {"input_tokens": 0, "output_tokens": 0},
            "modelUsage": {
                "big": {"inputTokens": 100, "outputTokens": 10},
                "small": {"inputTokens": 5, "outputTokens": 1}
            }
        });
        let u = parse_result_usage(&payload);
        assert_eq!((u.tokens_in, u.tokens_out), (105, 11));
    }

    #[test]
    fn a_result_with_neither_block_is_all_zero() {
        let u = parse_result_usage(&serde_json::json!({"type": "result"}));
        assert_eq!((u.tokens_in, u.tokens_out, u.cost_usd), (0, 0, 0.0));
    }
}

#[cfg(test)]
mod runner_kind_tests {
    use super::*;

    pub(super) fn actor_with(runner_cmd: Option<&str>) -> SessionActor {
        let spec: SessionSpec = serde_json::from_str(
            r#"{"session_id":"00000000-0000-0000-0000-000000000001",
                "sandbox_name":"ses-test","image":"puku-agent:latest",
                "cpus":2,"memory_mib":2048,"idle_timeout_s":600,
                "max_duration_s":3600,"prompt":"hi"}"#,
        )
        .unwrap();
        SessionActor {
            spec,
            state_dir: PathBuf::from("/tmp"),
            up_tx: tokio::sync::mpsc::unbounded_channel().0,
            sessions: SessionMap::default(),
            runner_cmd: runner_cmd.map(str::to_string),
            recovered: false,
            multi_tenant: false,
            egress_allow: Vec::new(),
            secret_hosts: Vec::new(),
            secret_env_injection: false,
            uploader: crate::uploader::Uploader::new(tokio::sync::mpsc::unbounded_channel().0),
            git_tokens: crate::gitpush::GitTokens::new(tokio::sync::mpsc::unbounded_channel().0),
            backend: Arc::new(crate::vm::fake::FakeBackend::new(puku_cloud_proto::Engine::Libkrun)),
            volumes: crate::volumes::SessionVolumes::Local,
        }
    }

    /// The regression this helper exists to prevent.
    ///
    /// The old test was `runner_cmd.is_some()`, which said "bash" for every
    /// production session the moment the SDK runner became the default —
    /// delivering a `control_request` that runner.mjs hands to the SDK as a
    /// user message. It would have passed a trial-worker gate and regressed
    /// on rollout.
    #[test]
    fn the_default_matches_the_mechanism_chosen_for_it() {
        let a = actor_with(None);
        assert_eq!(a.runner_command(), DEFAULT_RUNNER_CMD);
        // The pairing is the point, not which way it currently points: the
        // default and the interrupt mechanism must never disagree. Flipping
        // DEFAULT_RUNNER_CMD alone keeps this test honest.
        assert_eq!(a.uses_sdk_runner(), DEFAULT_RUNNER_CMD.contains("runner.mjs"));
    }

    /// Falling back is one env var, and the bash mechanism must come back
    /// with it.
    #[test]
    fn an_override_to_the_bash_runner_selects_the_bash_mechanism() {
        let a = actor_with(Some("exec /usr/local/bin/puku-runner"));
        assert!(!a.uses_sdk_runner());
    }

    /// The fake-CLI override used by the offline sweep wraps the SDK runner
    /// in `env`; it is still the SDK runner and must still interrupt as one.
    #[test]
    fn a_wrapped_sdk_override_is_still_the_sdk_runner() {
        let a = actor_with(Some(
            "exec env PUKU_CLI_PATH=/opt/puku/fake-puku-cli.sh node /opt/puku/runner.mjs",
        ));
        assert!(a.uses_sdk_runner());
    }

    /// A stub that is neither runner (README's echo smoke test) must not be
    /// mistaken for the SDK.
    #[test]
    fn a_stub_override_is_not_the_sdk_runner() {
        let a = actor_with(Some("echo '{\"type\":\"result\"}' >> /session/events.ndjson"));
        assert!(!a.uses_sdk_runner());
    }
}
