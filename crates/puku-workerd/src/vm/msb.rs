//! The libkrun engine, through the embedded microsandbox SDK.
//!
//! Everything here is what `session_actor.rs` used to do inline, moved behind
//! the [`VmBackend`] seam without changing what it does: same builder calls,
//! same egress policy construction, same detached process model.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use microsandbox::sandbox::ExecOptionsBuilder;
use microsandbox::{ExecEvent, Sandbox};
use puku_cloud_proto::data_proto::MAX_EXEC_OUTPUT_BYTES;
use puku_cloud_proto::Engine;

use super::{ExecOutput, ExecRequest, GuestIo, StreamOutcome, Vm, VmBackend, VmSpec, TIMEOUT_EXIT_CODE};

/// A free loopback port to publish a guest port on. msb binds it when the
/// VM starts; the gap between here and there is a race another process
/// could win, in which case the create fails and is reported as such.
fn free_loopback_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).context("finding a free host port")?;
    Ok(l.local_addr()?.port())
}

/// The SDK this binary embeds. Kept by hand in step with `Cargo.toml` and
/// `deploy/scripts/prestage-msb.sh` -- the three must agree, and this is the
/// one the control plane gets to see.
pub const SDK_VERSION: &str = "0.6.9";

/// Page size for inventory listing; the SDK's own maximum.
const LIST_PAGE: u32 = microsandbox::sandbox::MAX_SANDBOX_LIST_LIMIT;

pub struct MsbBackend;

impl MsbBackend {
    /// `home` overrides the microsandbox home (sandbox metadata, image
    /// cache). `None` leaves the SDK to resolve `MSB_HOME` itself, which is
    /// what the systemd unit relies on.
    pub fn new(home: Option<PathBuf>) -> Self {
        if let Some(home) = home {
            let backend = microsandbox::LocalBackend::builder().home(home).build_lazy();
            microsandbox::set_default_backend(backend);
        }
        MsbBackend
    }
}

/// Domain-suffix allowlist as an msb policy.
///
/// NOT `NetworkPolicy::default()`: that is `from_profiles([Public])`, which
/// carries a blanket allow-egress-to-the-internet rule, so every domain added
/// below was purely additive and the allowlist denied nothing. An empty
/// profile set is deny-by-default, which makes these domains the only
/// permitted egress. The DomainSuffix rules also authorize their own DNS
/// queries (see NetworkPolicy::evaluate_dns_query), so no separate DNS
/// allowance is needed.
pub(crate) fn egress_policy(domains: &[String]) -> microsandbox::NetworkPolicy {
    let mut policy =
        microsandbox::NetworkPolicy::from_profiles(Vec::<microsandbox::NetworkProfile>::new());
    for d in domains {
        match policy.clone().allow_domain_suffix(d.as_str()) {
            Ok(p) => policy = p,
            Err(e) => tracing::warn!(domain = %d, error = %e, "bad egress domain"),
        }
    }
    policy
}

#[async_trait]
impl VmBackend for MsbBackend {
    fn engine(&self) -> Engine {
        Engine::Libkrun
    }

    fn version(&self) -> String {
        format!("msb-{SDK_VERSION}")
    }

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>> {
        let mut builder = Sandbox::builder(&spec.name)
            .image(spec.image.as_str())
            .cpus(spec.cpus)
            .memory(spec.memory_mib);
        for m in &spec.mounts {
            let host = m.host.display().to_string();
            builder = builder.volume(m.guest.as_str(), move |v| v.bind(host));
        }
        for (k, v) in &spec.labels {
            builder = builder.label(k.as_str(), v.as_str());
        }
        if let Some(secs) = spec.max_duration_s {
            builder = builder.max_duration(secs);
        }
        // Published on loopback only: the listener lives in the msb process
        // for the VM's lifetime, and nothing but this worker should reach it.
        for guest in &spec.ports {
            builder = builder.port_bind(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                free_loopback_port()?,
                *guest,
            );
        }
        builder = builder.detached(true);

        if spec.multi_tenant {
            builder =
                builder.deployment_profile(microsandbox_types::DeploymentProfile::MultiTenant);
        }
        if !spec.egress_allow.is_empty() {
            let policy = egress_policy(&spec.egress_allow);
            builder = builder.network(move |n| n.policy(policy));
        }
        for (k, v) in &spec.env {
            builder = builder.env(k.as_str(), v.as_str());
        }
        for secret in &spec.secrets {
            let secret = secret.clone();
            builder = builder.secret(move |mut s| {
                s = s.env(secret.var.as_str()).value(secret.value.clone());
                for h in &secret.hosts {
                    s = if h.starts_with('*') {
                        s.allow_host_pattern(h.clone())
                    } else {
                        s.allow_host(h.clone())
                    };
                }
                s
            });
        }
        let sandbox = builder.create_detached().await.context("creating sandbox")?;
        Ok(Box::new(MsbVm { sandbox }))
    }

    async fn attach(&self, name: &str) -> Result<Box<dyn Vm>> {
        let handle = Sandbox::get(name).await.context("sandbox gone after restart")?;
        let sandbox = handle.connect().await.context("reattach failed")?;
        Ok(Box::new(MsbVm { sandbox }))
    }

    async fn remove(&self, name: &str) -> Result<()> {
        Sandbox::remove(name).await?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<String>> {
        // Every page. `Sandbox::list()` is the first page only, so on a host
        // with more sandboxes than one page the heartbeat under-reported and
        // /v1/fleet called the rest "vanished".
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        // A guard, not a limit anyone should hit: a backend that kept
        // handing back a cursor would otherwise loop the heartbeat for ever.
        for _ in 0..1000 {
            let c = cursor.take();
            let page = Sandbox::list_with(move |l| {
                let l = l.limit(LIST_PAGE);
                match c {
                    Some(c) => l.cursor(c),
                    None => l,
                }
            })
            .await?;
            names.extend(page.sandboxes.iter().map(|s| s.name().to_string()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(names)
    }

    async fn prepull(&self, image: &str) -> Result<()> {
        // msb has no pull-only call, so boot a throwaway VM on the image and
        // tear it down: the pull is the side effect that matters.
        let name = format!("prepull-{}", uuid::Uuid::new_v4().simple());
        let sb = Sandbox::builder(&name).image(image).memory(512u32).create().await?;
        let _ = sb.stop().await;
        let _ = Sandbox::remove(&name).await;
        Ok(())
    }
}

struct MsbVm {
    sandbox: Sandbox,
}

fn apply(mut e: ExecOptionsBuilder, req: &ExecRequest) -> ExecOptionsBuilder {
    e = e.args(req.args.iter().cloned());
    if let Some(cwd) = &req.cwd {
        e = e.cwd(cwd.clone());
    }
    if let Some(user) = &req.user {
        e = e.user(user.clone());
    }
    if !req.env.is_empty() {
        e = e.envs(req.env.iter().cloned());
    }
    if let Some(t) = req.timeout {
        e = e.timeout(t);
    }
    e
}

#[async_trait]
impl Vm for MsbVm {
    fn name(&self) -> &str {
        self.sandbox.name()
    }

    async fn exec(&self, req: ExecRequest) -> Result<ExecOutput> {
        let result = match &req.stdin {
            None => self.sandbox.exec_with(req.cmd.clone(), |e| apply(e, &req)).await,
            Some(bytes) => {
                let mut h = self
                    .sandbox
                    .exec_stream_with(req.cmd.clone(), |e| apply(e, &req).stdin_pipe())
                    .await?;
                let stdin = h.take_stdin().context("no stdin pipe")?;
                stdin.write(bytes).await?;
                stdin.close().await?;
                match req.timeout {
                    Some(t) => match tokio::time::timeout(t, h.collect()).await {
                        Ok(r) => r,
                        Err(_) => {
                            let _ = h.kill().await;
                            Err(microsandbox::MicrosandboxError::ExecTimeout(t))
                        }
                    },
                    None => h.collect().await,
                }
            }
        };
        let out = match result {
            Ok(out) => out,
            // msb kills the command and reports the timeout as an error;
            // callers want the shell convention instead.
            Err(microsandbox::MicrosandboxError::ExecTimeout(t)) => {
                return Ok(ExecOutput {
                    code: TIMEOUT_EXIT_CODE,
                    stdout: Vec::new(),
                    stderr: format!("command timed out after {} ms\n", t.as_millis()).into_bytes(),
                    timed_out: true,
                })
            }
            Err(e) => return Err(e.into()),
        };
        let status = out.status();
        Ok(ExecOutput {
            code: status.code,
            stdout: out.stdout_bytes().to_vec(),
            stderr: out.stderr_bytes().to_vec(),
            timed_out: false,
        })
    }

    async fn exec_stream(
        &self,
        req: ExecRequest,
        stdin: Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
        stdout: tokio::sync::mpsc::Sender<bytes::Bytes>,
    ) -> Result<StreamOutcome> {
        let piped = stdin.is_some();
        let mut h = self
            .sandbox
            .exec_stream_with(req.cmd.clone(), |e| {
                let e = apply(e, &req);
                if piped {
                    e.stdin_pipe()
                } else {
                    e
                }
            })
            .await?;
        if let Some(mut src) = stdin {
            let sink = h.take_stdin().context("no stdin pipe")?;
            tokio::spawn(async move {
                while let Some(chunk) = src.recv().await {
                    if sink.write(&chunk).await.is_err() {
                        break;
                    }
                }
                let _ = sink.close().await;
            });
        }
        let mut stderr = Vec::new();
        let pump = async {
            loop {
                match h.recv().await {
                    Some(ExecEvent::Stdout(b)) => {
                        // A reader that went away cannot be told anything
                        // more; stop producing for it.
                        if stdout.send(b).await.is_err() {
                            return Ok(-1);
                        }
                    }
                    Some(ExecEvent::Stderr(b)) => {
                        let room = MAX_EXEC_OUTPUT_BYTES.saturating_sub(stderr.len());
                        stderr.extend_from_slice(&b[..b.len().min(room)]);
                    }
                    Some(ExecEvent::Exited { code }) => return Ok(code),
                    Some(ExecEvent::Failed(f)) => anyhow::bail!("the command could not start: {f:?}"),
                    Some(_) => {}
                    None => anyhow::bail!("the command's output ended without an exit status"),
                }
            }
        };
        let (code, timed_out) = match req.timeout {
            Some(t) => match tokio::time::timeout(t, pump).await {
                Ok(r) => (r?, false),
                Err(_) => (TIMEOUT_EXIT_CODE, true),
            },
            None => (pump.await?, false),
        };
        if timed_out || code == -1 {
            let _ = h.kill().await;
        }
        Ok(StreamOutcome { code, stderr, timed_out })
    }

    async fn connect_port(&self, port: u16) -> Result<Box<dyn GuestIo>> {
        let published = self
            .sandbox
            .config()
            .spec
            .network
            .ports
            .iter()
            .find(|p| p.guest_port == port)
            .map(|p| (p.host_bind.clone(), p.host_port))
            .with_context(|| format!("guest port {port} was not published for this VM"))?;
        let host = if published.0.is_empty() { "127.0.0.1".to_string() } else { published.0 };
        let stream = tokio::net::TcpStream::connect((host.as_str(), published.1))
            .await
            .with_context(|| format!("connecting to guest port {port}"))?;
        stream.set_nodelay(true).ok();
        Ok(Box::new(stream))
    }

    async fn stop(&self) -> Result<()> {
        self.sandbox.stop().await?;
        Ok(())
    }
}

#[cfg(test)]
mod egress_policy_tests {
    use super::egress_policy;
    use microsandbox::NetworkPolicy;

    fn allowlist_policy(domains: &[&str]) -> NetworkPolicy {
        egress_policy(&domains.iter().map(|d| d.to_string()).collect::<Vec<_>>())
    }

    fn as_json(policy: &NetworkPolicy) -> serde_json::Value {
        serde_json::to_value(policy).unwrap()
    }

    /// The bug this guards. `NetworkPolicy::default()` is
    /// `from_profiles([Public])`, which carries a blanket allow-egress rule
    /// for the public internet; building the allowlist on top of it left
    /// every destination reachable.
    #[test]
    fn default_policy_carries_a_blanket_public_allow() {
        let json = as_json(&NetworkPolicy::default()).to_string();
        assert!(
            json.contains("public"),
            "default() is expected to allow the public internet: {json}"
        );
    }

    #[test]
    fn allowlist_policy_denies_by_default() {
        let json = as_json(&allowlist_policy(&["puku.sh"]));
        assert_eq!(json["default_egress"], "deny");
    }

    /// No catch-all group rule may survive, or the domain rules are inert.
    #[test]
    fn allowlist_policy_has_no_blanket_allow() {
        let json = as_json(&allowlist_policy(&["puku.sh", "github.com"])).to_string();
        assert!(!json.contains("public"), "blanket public allow leaked in: {json}");
        assert!(json.contains("puku.sh"), "allowlisted domain missing: {json}");
        assert!(json.contains("github.com"), "allowlisted domain missing: {json}");
    }

    /// Every rule must be a domain rule -- the only egress permitted.
    #[test]
    fn allowlist_policy_rules_are_all_domain_scoped() {
        let json = as_json(&allowlist_policy(&["puku.sh", "github.com"]));
        let rules = json["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2, "unexpected rule set: {rules:?}");
        for rule in rules {
            let dest = rule["destination"].to_string();
            assert!(
                dest.contains("puku.sh") || dest.contains("github.com"),
                "non-domain rule present: {dest}"
            );
        }
    }
}
