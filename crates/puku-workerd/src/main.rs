mod controlplane;
mod datalink;
mod gitpush;
mod hostcap;
mod idle;
mod machines;
mod session_actor;
mod snapshot;
mod uploader;
mod vm;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

/// puku-agent-cloud worker daemon: embeds the microsandbox runtime and runs
/// one microVM per assigned session.
#[derive(Parser, Debug, Clone)]
#[command(name = "puku-workerd")]
pub struct Args {
    #[arg(long, env = "PUKU_CONTROLD_URL", default_value = "ws://127.0.0.1:7770/v1/worker")]
    controld_url: String,
    /// The data-plane endpoint machine traffic rides on. Defaults to
    /// PUKU_CONTROLD_URL with `/data` appended, which is right whenever that
    /// URL ends in `/v1/worker`.
    #[arg(long, env = "PUKU_DATA_URL")]
    data_url: Option<String>,
    #[arg(long, env = "PUKU_WORKER_NAME", default_value = "worker-1")]
    worker_name: String,
    #[arg(long, env = "PUKU_WORKER_TOKEN", default_value = "dev-worker-token")]
    worker_token: String,
    #[arg(long, env = "PUKU_WORKER_TOKEN_FILE")]
    worker_token_file: Option<String>,
    /// Root for per-session volume directories.
    #[arg(long, env = "PUKU_STATE_DIR", default_value = "/var/lib/puku")]
    state_dir: PathBuf,
    /// Override the microsandbox home (sandbox metadata, image cache, ...).
    /// Defaults to the SDK default (~/.microsandbox).
    #[arg(long, env = "PUKU_MSB_HOME")]
    msb_home: Option<PathBuf>,
    /// Concurrent sessions this host accepts. 0 (the default) sizes it
    /// from the machine's own cores and memory and keeps re-sizing it as
    /// memory frees, which is almost always better than a number typed in
    /// once: too high and the box OOM-kills somebody's paid run, too low
    /// and most of it sits idle while sessions queue.
    #[arg(long, env = "PUKU_CAPACITY_SLOTS", default_value_t = 0)]
    capacity_slots: u32,
    /// Dev/testing: run this shell command in the guest instead of
    /// `puku-runner` (e.g. a stub that writes fake events).
    #[arg(long, env = "PUKU_RUNNER_CMD")]
    pub runner_cmd: Option<String>,
    /// Apply the multi-tenant isolation floor to every sandbox (M3;
    /// production workers set this).
    #[arg(
        long,
        env = "PUKU_MULTI_TENANT",
        default_value_t = false,
        value_parser = parse_bool_flag
    )]
    pub multi_tenant: bool,
    /// Comma-separated egress domain-suffix allowlist. When set, sandbox
    /// egress is default-deny except these domains. Left empty under
    /// `--multi-tenant`, DEFAULT_EGRESS_ALLOW applies instead of no policy.
    #[arg(long, env = "PUKU_EGRESS_ALLOW", default_value = "")]
    pub egress_allow_raw: String,
    /// Comma-separated hosts the model credential may be injected into at
    /// the network boundary. `*.` prefixes are treated as wildcards. The
    /// default is puku-cli's completions endpoint (`PUKU_AI_BASE_URL`).
    /// Attach no network policy at all, even under --multi-tenant, so the
    /// guest can reach any host.
    ///
    /// The microVM is a hardware isolation boundary, so this does not let
    /// the agent touch the host or other sessions. What it does remove is
    /// the *exfiltration* boundary: a prompt injection from a fetched page
    /// can POST the workspace anywhere. That is an acceptable trade on a
    /// single-tenant box you own and a bad one when you are running other
    /// people's code, which is why it is explicit rather than implied.
    #[arg(long, env = "PUKU_EGRESS_UNRESTRICTED", default_value_t = false)]
    pub egress_unrestricted: bool,
    #[arg(long, env = "PUKU_SECRET_HOSTS", default_value = "api-cli.puku.sh")]
    pub secret_hosts_raw: String,
    /// Opt in to network-boundary secret injection for the model credential
    /// (guest sees only a `$MSB_…` placeholder). Off by default: msb's
    /// injection MITMs guest TLS via NODE_EXTRA_CA_CERTS, and puku-cli's
    /// networking stalls on its puku.sh phone-homes under that proxy and
    /// exits silently. Flip the default back once the two are compatible.
    #[arg(
        long,
        env = "PUKU_SECRET_ENV_INJECTION",
        default_value_t = false,
        value_parser = parse_bool_flag
    )]
    pub secret_env_injection: bool,
    /// Deprecated: plain-env delivery is now the default; this flag has no
    /// effect. Kept so existing units don't crash-loop on an unknown flag.
    #[arg(
        long,
        env = "PUKU_INSECURE_PLAIN_KEY",
        default_value_t = false,
        value_parser = parse_bool_flag,
        hide = true
    )]
    pub insecure_plain_key: bool,
    /// Images to pre-pull at startup so first sessions boot warm.
    #[arg(long, env = "PUKU_PREPULL_IMAGES", default_value = "")]
    pub prepull_images: String,
    /// Run sessions under libkrun (microsandbox). On by default: it is what
    /// every worker ran before engines existed.
    #[arg(
        long,
        env = "PUKU_ENGINE_LIBKRUN",
        default_value_t = true,
        value_parser = parse_bool_flag,
        action = clap::ArgAction::Set
    )]
    pub engine_libkrun: bool,
    /// Run sessions under Cloud Hypervisor. Off by default; Linux/KVM only.
    /// Advertised to controld only if this host actually passes the engine's
    /// runtime checks, so switching it on cannot attract work the box
    /// cannot run.
    #[arg(
        long,
        env = "PUKU_ENGINE_CLOUD_HYPERVISOR",
        default_value_t = false,
        value_parser = parse_bool_flag,
        action = clap::ArgAction::Set
    )]
    pub engine_cloud_hypervisor: bool,
    /// Cloud Hypervisor toolchain, as deploy/scripts/prestage-ch.sh lays it
    /// out.
    #[arg(long, env = "PUKU_CH_BIN", default_value = "/opt/puku/ch/bin/cloud-hypervisor")]
    pub ch_bin: PathBuf,
    #[arg(long, env = "PUKU_CH_VIRTIOFSD", default_value = "/opt/puku/ch/bin/virtiofsd")]
    pub ch_virtiofsd: PathBuf,
    /// Uncompressed guest kernel (`vmlinux` on x86_64, `Image` on aarch64).
    #[arg(long, env = "PUKU_CH_KERNEL", default_value = "/opt/puku/ch/vmlinux")]
    pub ch_kernel: PathBuf,
    /// Staged guest disks (deploy/scripts/build-ch-rootfs.sh). Defaults to
    /// `<state_dir>/images`.
    #[arg(long, env = "PUKU_CH_IMAGES_DIR")]
    pub ch_images_dir: Option<PathBuf>,
    /// Size of each VM's writable disk, in GiB. Sparse.
    #[arg(long, env = "PUKU_CH_UPPER_GIB", default_value_t = 16)]
    pub ch_upper_gib: u64,
    /// The guest's /dev/shm. Chromium in a computer image wants a lot.
    #[arg(long, env = "PUKU_CH_SHM", default_value = "512m")]
    pub ch_shm: String,
    /// Snapshot captures and restores this worker runs at once. Each holds
    /// up to two upload parts in memory.
    #[arg(long, env = "PUKU_SNAPSHOT_CONCURRENCY", default_value_t = 2)]
    pub snapshot_concurrency: usize,
    /// zstd level for snapshot layers: 3 is fast and still shrinks a home a
    /// lot; 19 is small and slow.
    #[arg(long, env = "PUKU_SNAPSHOT_ZSTD_LEVEL", default_value_t = 3)]
    pub snapshot_zstd_level: i32,
    /// Cloud Hypervisor: give memory a guest frees back to the host
    /// (virtio-balloon free page reporting), so an idle desktop does not keep
    /// its peak for ever. Needs CONFIG_VIRTIO_BALLOON and CONFIG_PAGE_REPORTING
    /// in the guest kernel; verify on the host before relying on it.
    #[arg(
        long,
        env = "PUKU_CH_FREE_PAGE_REPORTING",
        default_value_t = false,
        value_parser = parse_bool_flag,
        action = clap::ArgAction::Set
    )]
    pub ch_free_page_reporting: bool,
    /// vCPUs per host core that capacity counts. 1.0 never overcommits;
    /// desktops idle most of the time, so 2.0 packs twice as many where
    /// memory allows. Memory is never overcommitted.
    #[arg(long, env = "PUKU_CPU_OVERCOMMIT", default_value_t = 1.0)]
    pub cpu_overcommit: f64,
}

/// clap's stock bool parser takes only `true`/`false`, but these arrive as
/// environment variables, where `1` is the obvious thing to write — and the
/// docs said to. Accept the usual spellings rather than crash-looping the
/// unit on a value everyone reasonably expects to work.
fn parse_bool_flag(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" | "" => Ok(false),
        other => Err(format!(
            "expected a boolean (1/0, true/false, yes/no, on/off), got {other:?}"
        )),
    }
}

/// Applied when a multi-tenant worker names no allowlist of its own. A
/// single suffix covers puku-cli's whole surface: api-cli (completions),
/// puku.sh (oauth/account), mcp-proxy, and puku-cli.relay.
/// Beyond that it covers the package registries real work needs. Note the
/// pairs -- `pypi.org` serves the index but wheels download from
/// `pythonhosted.org`, and cargo fetches from `index.crates.io` -- so a
/// list naming only the headline domain breaks installs halfway through.
pub const DEFAULT_EGRESS_ALLOW: &str = "puku.sh,\
    github.com,githubusercontent.com,\
    npmjs.org,jsr.io,\
    pypi.org,pythonhosted.org,\
    crates.io,golang.org,\
    ubuntu.com,debian.org";

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Args {
    /// The domain suffixes a session may reach. An empty list means no
    /// policy is attached, which means unrestricted.
    pub fn egress_allow(&self) -> Vec<String> {
        // An explicit opt-in beats everything, including the multi-tenant
        // floor: an operator who sets this has said what they want.
        if self.egress_unrestricted {
            return Vec::new();
        }
        let explicit = split_csv(&self.egress_allow_raw);
        // Never leave a multi-tenant worker with unrestricted egress just
        // because the operator forgot the variable.
        if explicit.is_empty() && self.multi_tenant {
            return split_csv(DEFAULT_EGRESS_ALLOW);
        }
        explicit
    }

    pub fn secret_hosts(&self) -> Vec<String> {
        split_csv(&self.secret_hosts_raw)
    }

    pub fn data_url(&self) -> String {
        self.data_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| format!("{}/data", self.controld_url.trim_end_matches('/')))
    }
}

/// See the note on puku-controld's `main`: Sentry starts before the runtime
/// so the panic hook exists before any session actor does.
/// Both of rustls' crypto backends end up compiled in -- ring through our
/// reqwest, aws-lc-rs through microsandbox's -- so rustls cannot pick one on
/// its own, and the first TLS connection that leans on the process default
/// panics. That is tokio-tungstenite's, whenever controld is a wss:// URL: a
/// worker on any host but controld's own. Choose one before anything connects.
fn install_tls_provider() {
    // Err only means a provider is already installed, which is all this wants.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn main() -> anyhow::Result<()> {
    install_tls_provider();
    let args = Args::parse();

    let _guard = puku_observability::init("puku-workerd", traces_sampler);

    // Set once on the main hub. Every child hub is cloned from this one, so
    // `worker` lands on every event the process ever sends -- including
    // events from session actors that outlive this frame.
    sentry::configure_scope(|scope| scope.set_tag("worker", &args.worker_name));

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(run(args));
    drop(rt);
    result
}

/// Volume here is bounded by `capacity_slots` -- tens, not thousands -- so
/// session work is sampled in full. microVM boot latency is exactly the kind
/// of thing a waterfall answers, and the reconnect loop is the only thing
/// that ticks often enough to matter.
fn traces_sampler(ctx: &sentry::TransactionContext) -> f32 {
    if let Some(sampled) = ctx.sampled() {
        return if sampled { 1.0 } else { 0.0 };
    }
    match ctx.name() {
        "control_plane_link" => 0.005,
        _ => 1.0,
    }
}

async fn run(mut args: Args) -> anyhow::Result<()> {

    if args.insecure_plain_key {
        tracing::warn!(
            "PUKU_INSECURE_PLAIN_KEY is deprecated and has no effect: plain-env \
             credential delivery is now the default (see --secret-env-injection)"
        );
    }

    if let Some(path) = &args.worker_token_file {
        args.worker_token = std::fs::read_to_string(path)?.trim().to_string();
    }

    let backends = build_backends(&args)?;
    tracing::info!(engines = %backends.version_string(), "engines enabled");

    std::fs::create_dir_all(args.state_dir.join("sessions"))?;

    // Warm every engine's image cache so first sessions don't pay the pull.
    for image in args
        .prepull_images
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        for backend in backends.all() {
            let image = image.to_string();
            tokio::spawn(async move {
                tracing::info!(%image, engine = %backend.engine(), "pre-pulling image");
                match backend.prepull(&image).await {
                    Ok(()) => tracing::info!(%image, engine = %backend.engine(), "image warm"),
                    Err(e) => tracing::warn!(
                        %image, engine = %backend.engine(), error = format!("{e:#}"),
                        "pre-pull failed"
                    ),
                }
            });
        }
    }

    // Arc, not Clone: `Link` owns the receiver half of the up-channel and
    // `run_once` guards against concurrent entry, so duplicating it would be
    // wrong. The supervisor needs a handle per attempt, not a copy.
    let link = std::sync::Arc::new(controlplane::Link::new(args.clone(), backends));
    // Reattach to sessions whose VMs survived a restart before registering,
    // so the Register frame lists them and controld returns their cursors.
    link.reconcile_from_disk();
    link.start_machines().await;
    // Supervised rather than a bare loop: a panic in here severs the worker
    // from the control plane permanently, and the only symptom is sessions
    // that never get assigned.
    puku_observability::supervise("control_plane_link", move || {
        let link = link.clone();
        async move {
            loop {
                if let Err(e) = link.run_once().await {
                    // {:#}: the cause, not just "connecting to controld".
                    tracing::warn!(error = format!("{e:#}"), "control-plane link dropped; reconnecting in 3s");
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    });
    std::future::pending::<()>().await;
    Ok(())
}

/// The engines the operator switched on, as this host can actually run them.
///
/// A worker with none is refused at startup: it would register, advertise
/// nothing, and sit in the fleet looking healthy while never taking work.
fn build_backends(args: &Args) -> anyhow::Result<vm::Backends> {
    let mut backends = vm::Backends::default();
    if args.engine_libkrun {
        backends = backends.with(std::sync::Arc::new(vm::msb::MsbBackend::new(args.msb_home.clone())));
    }
    if args.engine_cloud_hypervisor {
        if args.secret_env_injection {
            // msb's injection is a TLS proxy inside its own network stack;
            // Cloud Hypervisor guests have none. Advertising the engine would
            // quietly hand sessions their credential as plain env instead.
            tracing::error!(
                "PUKU_ENGINE_CLOUD_HYPERVISOR is set together with PUKU_SECRET_ENV_INJECTION, \
                 which Cloud Hypervisor cannot do; not advertising it"
            );
        } else {
            let cfg = vm::ch::ChConfig {
                ch_bin: args.ch_bin.clone(),
                virtiofsd_bin: args.ch_virtiofsd.clone(),
                kernel: args.ch_kernel.clone(),
                images_dir: args.ch_images_dir.clone().unwrap_or_else(|| args.state_dir.join("images")),
                vms_dir: args.state_dir.join("vms"),
                upper_gib: args.ch_upper_gib.max(1),
                shm: args.ch_shm.clone(),
                free_page_reporting: args.ch_free_page_reporting,
            };
            match vm::ch::ChBackend::probe(cfg) {
                Ok(ch) => backends = backends.with(std::sync::Arc::new(ch)),
                // Switching the flag on must not attract work the box cannot
                // run: say why, and leave the engine unadvertised.
                Err(e) => tracing::error!(
                    error = format!("{e:#}"),
                    "PUKU_ENGINE_CLOUD_HYPERVISOR is set but this host cannot run it; not advertising it"
                ),
            }
        }
    }
    if backends.is_empty() {
        anyhow::bail!(
            "no VM engine is enabled on this worker: set PUKU_ENGINE_LIBKRUN or \
             PUKU_ENGINE_CLOUD_HYPERVISOR"
        );
    }
    Ok(backends)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a chosen provider, building a TLS client -- which a wss://
    /// controld connection does -- panics, because both backends are in.
    #[test]
    fn a_tls_client_builds_once_the_provider_is_installed() {
        install_tls_provider();
        install_tls_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        let _ = rustls::ClientConfig::builder();
    }

    fn args_with(egress: &str, multi_tenant: bool) -> Args {
        Args {
            controld_url: String::new(),
            data_url: None,
            worker_name: String::new(),
            worker_token: String::new(),
            worker_token_file: None,
            state_dir: PathBuf::from("/tmp"),
            msb_home: None,
            capacity_slots: 1,
            runner_cmd: None,
            multi_tenant,
            egress_allow_raw: egress.to_string(),
            secret_hosts_raw: "api-cli.puku.sh".to_string(),
            egress_unrestricted: false,
            insecure_plain_key: false,
            secret_env_injection: false,
            prepull_images: String::new(),
            engine_libkrun: true,
            engine_cloud_hypervisor: false,
            ch_bin: PathBuf::from("/nonexistent/cloud-hypervisor"),
            ch_virtiofsd: PathBuf::from("/nonexistent/virtiofsd"),
            ch_kernel: PathBuf::from("/nonexistent/vmlinux"),
            ch_images_dir: None,
            ch_upper_gib: 16,
            ch_shm: "512m".into(),
            snapshot_concurrency: 2,
            snapshot_zstd_level: 3,
            ch_free_page_reporting: false,
            cpu_overcommit: 1.0,
        }
    }

    /// Asking for Cloud Hypervisor on a host that cannot run it leaves the
    /// worker on libkrun rather than advertising an engine that will fail.
    #[test]
    fn an_unrunnable_cloud_hypervisor_is_not_advertised() {
        let mut args = args_with("", false);
        args.engine_cloud_hypervisor = true;
        let b = build_backends(&args).unwrap();
        assert_eq!(b.engines(), vec![puku_cloud_proto::Engine::Libkrun]);
    }

    /// Existing units set neither variable and must keep running libkrun.
    #[test]
    fn libkrun_is_on_and_cloud_hypervisor_off_by_default() {
        let args = Args::try_parse_from(["puku-workerd"]).unwrap();
        assert!(args.engine_libkrun);
        assert!(!args.engine_cloud_hypervisor);
    }

    /// Each engine is a plain on/off switch, in either direction.
    #[test]
    fn each_engine_can_be_switched_either_way() {
        let args = Args::try_parse_from([
            "puku-workerd",
            "--engine-libkrun",
            "false",
            "--engine-cloud-hypervisor",
            "1",
        ])
        .unwrap();
        assert!(!args.engine_libkrun);
        assert!(args.engine_cloud_hypervisor);
    }

    #[test]
    fn the_data_url_follows_the_control_url() {
        let mut args = args_with("", false);
        args.controld_url = "wss://agent.api.puku.sh/v1/worker".into();
        assert_eq!(args.data_url(), "wss://agent.api.puku.sh/v1/worker/data");
        args.data_url = Some("wss://data.example/v1/worker/data".into());
        assert_eq!(args.data_url(), "wss://data.example/v1/worker/data");
    }

    /// A worker with every engine off would register and never take work.
    #[test]
    fn a_worker_with_no_engine_refuses_to_start() {
        let mut args = args_with("", false);
        args.engine_libkrun = false;
        assert!(build_backends(&args).is_err());
    }

    /// A multi-tenant worker must never end up with no egress policy just
    /// because the operator omitted the variable.
    /// The escape hatch must beat the multi-tenant floor — otherwise an
    /// operator who asked for open egress silently does not get it.
    #[test]
    fn unrestricted_overrides_even_the_multi_tenant_floor() {
        let mut args = args_with("", true);
        args.egress_unrestricted = true;
        assert!(
            args.egress_allow().is_empty(),
            "an empty allowlist is what attaches no policy at all"
        );
    }

    /// And it is off unless asked for: a multi-tenant worker must not end
    /// up open because of a default.
    #[test]
    fn unrestricted_is_off_by_default() {
        assert!(!args_with("", true).egress_unrestricted);
        assert!(!args_with("", true).egress_allow().is_empty());
    }

    #[test]
    fn multi_tenant_without_allowlist_falls_back_to_default() {
        let allow = args_with("", true).egress_allow();
        assert!(allow.contains(&"puku.sh".to_string()));
        assert!(allow.contains(&"github.com".to_string()));
    }

    #[test]
    fn single_tenant_without_allowlist_stays_unrestricted() {
        assert!(args_with("", false).egress_allow().is_empty());
    }

    #[test]
    fn explicit_allowlist_wins_over_default() {
        assert_eq!(
            args_with("example.com, foo.test ,", true).egress_allow(),
            vec!["example.com".to_string(), "foo.test".to_string()]
        );
    }

    #[test]
    fn secret_hosts_defaults_to_puku_completions_endpoint() {
        assert_eq!(
            args_with("", false).secret_hosts(),
            vec!["api-cli.puku.sh".to_string()]
        );
    }
}

#[cfg(test)]
mod bool_flag_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn accepts_the_spellings_operators_actually_write() {
        for s in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert_eq!(parse_bool_flag(s), Ok(true), "{s:?} should parse as true");
        }
        for s in ["0", "false", "no", "off", ""] {
            assert_eq!(parse_bool_flag(s), Ok(false), "{s:?} should parse as false");
        }
        assert!(parse_bool_flag("maybe").is_err());
    }

    /// The exact env value that crash-looped the unit. clap infers
    /// `SetTrue` for `bool`, so the flag takes no value on the command
    /// line, but an env var is still run through the value parser — which
    /// is where `PUKU_MULTI_TENANT=1` used to abort startup.
    #[test]
    fn multi_tenant_env_value_one_is_accepted() {
        std::env::set_var("PUKU_MULTI_TENANT", "1");
        let args = Args::try_parse_from(["puku-workerd"]).unwrap();
        std::env::remove_var("PUKU_MULTI_TENANT");
        assert!(args.multi_tenant);
        assert!(args.egress_allow().contains(&"puku.sh".to_string()));
    }
}

#[cfg(test)]
mod default_allowlist_tests {
    use super::*;

    /// The constant uses line continuations; a stray space would produce a
    /// domain like " github.com" that silently matches nothing.
    #[test]
    fn default_allowlist_parses_cleanly() {
        let domains = split_csv(DEFAULT_EGRESS_ALLOW);
        assert!(domains.len() >= 10, "unexpectedly short: {domains:?}");
        for d in &domains {
            assert_eq!(d.trim(), d, "whitespace in domain {d:?}");
            assert!(!d.contains(' '), "space inside domain {d:?}");
            assert!(d.contains('.'), "not a domain: {d:?}");
        }
    }

    /// The pairs that break package installs when only the headline domain
    /// is listed.
    #[test]
    fn default_allowlist_covers_download_hosts_not_just_indexes() {
        let d = split_csv(DEFAULT_EGRESS_ALLOW);
        for needed in ["pypi.org", "pythonhosted.org", "crates.io", "npmjs.org", "puku.sh"] {
            assert!(d.contains(&needed.to_string()), "missing {needed}: {d:?}");
        }
    }
}
