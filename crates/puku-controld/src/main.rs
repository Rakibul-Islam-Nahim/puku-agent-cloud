mod api;
mod archive;
mod blobstore;
mod connectors;
mod auth;
mod datalink;
mod db;
mod fakes3;
mod fence;
mod githubapp;
mod harness;
mod hostloss;
mod leases;
mod links;
mod memory;
mod inttests;
mod notify;
mod oauth;
mod recovery;
mod relay;
mod sampling;
mod scheduler;
mod secretbox;
mod skills;
mod snapshots;
mod sharedvol;
mod sweeper;
mod triggers;
mod workerlink;
mod workertoken;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// puku-agent-cloud control plane: REST API, client relay, worker link.
#[derive(Parser, Debug)]
#[command(name = "puku-controld")]
struct Args {
    #[arg(long, env = "PUKU_DATABASE_URL", default_value = "postgres://puku:puku@127.0.0.1:5432/puku_cloud")]
    database_url: String,
    /// Connections controld holds to its database. Keep it under the database's
    /// own limit: Supabase's session pooler allows 15 per pool, where the default
    /// of 16 fails under load as "max clients reached".
    #[arg(long, env = "PUKU_DATABASE_MAX_CONNECTIONS", default_value_t = 16)]
    database_max_connections: u32,
    /// Ceph pool the shared-volume workers keep session disks in. Set it,
    /// with Ceph credentials that may run `osd blocklist`, and a session
    /// whose host died moves to another host after that host is fenced.
    #[arg(long, env = "PUKU_RBD_POOL")]
    rbd_pool: Option<String>,
    #[arg(long, env = "PUKU_CEPH_USER", default_value = "puku")]
    ceph_user: String,
    #[arg(long, env = "PUKU_CEPH_CONF")]
    ceph_conf: Option<String>,
    #[arg(long, env = "PUKU_LISTEN_ADDR", default_value = "127.0.0.1:7770")]
    listen_addr: String,
    /// Shared secret workers present in Register. Read from
    /// PUKU_WORKER_TOKEN_FILE if set, else this value.
    #[arg(long, env = "PUKU_WORKER_TOKEN", default_value = "dev-worker-token")]
    worker_token: String,
    #[arg(long, env = "PUKU_WORKER_TOKEN_FILE")]
    worker_token_file: Option<String>,
    /// Keep accepting the legacy shared worker secret. Per-worker tokens
    /// (`gen-worker-token`) supersede it; this exists so a running fleet can
    /// be migrated host by host, and should be turned off once it is.
    #[arg(long, env = "PUKU_ALLOW_SHARED_WORKER_TOKEN", default_value_t = true)]
    allow_shared_worker_token: bool,
    /// Default guest image for new sessions.
    #[arg(long, env = "PUKU_AGENT_IMAGE", default_value = "puku-agent:latest")]
    agent_image: String,
    /// Puku platform API key injected into session specs, reaching the
    /// guest as `PUKU_AI_API_KEY` (workerd delivers it as a
    /// network-boundary secret unless PUKU_INSECURE_PLAIN_KEY).
    #[arg(long, env = "PUKU_AI_API_KEY")]
    puku_api_key: Option<String>,
    /// Deprecated alias for --puku-api-key. puku-cli talks to the puku
    /// platform, not Anthropic; kept so existing deployments keep working
    /// through one release.
    #[arg(long, env = "PUKU_ANTHROPIC_API_KEY", hide = true)]
    legacy_anthropic_api_key: Option<String>,
    /// Long-lived puku subscription token (`puku-cli setup-token`);
    /// alternative to a puku platform API key.
    #[arg(long, env = "PUKU_OAUTH_TOKEN")]
    puku_oauth_token: Option<String>,
    /// Path to a puku session file (~/.config/pukucode/session.json).
    /// Re-read at every dispatch so refreshes on this machine propagate.
    /// Alternative to --puku-api-key for OAuth subscription logins.
    #[arg(long, env = "PUKU_SESSION_FILE")]
    puku_session_file: Option<std::path::PathBuf>,
    /// Static git token (PAT) for repo clone/push. Superseded per-session
    /// by GitHub App installation tokens when the app is configured.
    ///
    /// Operator-wide, so it is ignored unless
    /// --allow-operator-credentials is set: any caller can name any repo,
    /// and this token would clone whatever it can read.
    #[arg(long, env = "PUKU_GIT_TOKEN")]
    git_token: Option<String>,
    /// Let operator-wide credentials reach a guest when the caller has none
    /// of their own: PUKU_AI_API_KEY, PUKU_OAUTH_TOKEN, PUKU_SESSION_FILE
    /// and the static PUKU_GIT_TOKEN.
    ///
    /// Off by default, because every one of those is a cross-tenant hole on
    /// a shared deployment. A model credential silently bills the operator
    /// for a caller who never stored one; a git PAT clones any repo it can
    /// read for any caller who names it. A session with nothing of its own
    /// is refused up front instead, naming what is missing.
    ///
    /// Turn it on for a single-tenant or development box, where "the
    /// operator" and "the user" are the same person.
    #[arg(long, env = "PUKU_ALLOW_OPERATOR_CREDENTIALS", default_value_t = false)]
    allow_operator_credentials: bool,
    /// OAuth issuer used to mint a short-lived bearer from a stored refresh
    /// token. The openauth client appends `/token`.
    #[arg(long, env = "PUKU_AUTH_ISSUER", default_value = "https://puku.sh/api/oauth")]
    auth_issuer: String,
    /// Puku platform API base. Bearer tokens are verified against
    /// `{}/auth/verify` here, exactly as puku-chat-compute-service does.
    #[arg(long, env = "PUKU_API_URL", default_value = "https://chat.api.puku.sh")]
    api_url: String,
    /// Model gateway the guest routes through. Defaults to PUKU_API_URL.
    /// puku-cowork points its VMs at the same host, so a cloud session and
    /// a desktop session bill through the same path.
    #[arg(long, env = "PUKU_AI_BASE_URL")]
    puku_ai_base_url: Option<String>,
    /// Accept puku platform bearers in addition to `pkc_` API keys. This is
    /// what lets Puku Desktop and the web app call this API at all.
    #[arg(long, env = "PUKU_PLATFORM_AUTH", default_value_t = true)]
    platform_auth: bool,
    /// Memory service (puku-memory-service). Unset means sessions get no
    /// memory, which is the pre-memory behaviour exactly.
    #[arg(long, env = "PUKU_MEMORY_URL")]
    memory_url: Option<String>,
    /// Server-to-server credential for the memory service.
    #[arg(long, env = "PUKU_MEMORY_SERVICE_KEY")]
    memory_service_key: Option<String>,
    /// Cloudflare Access service token, when memory is reached over a public
    /// hostname instead of the compose network. Both halves or neither.
    #[arg(long, env = "PUKU_MEMORY_ACCESS_CLIENT_ID")]
    memory_access_client_id: Option<String>,
    #[arg(long, env = "PUKU_MEMORY_ACCESS_CLIENT_SECRET")]
    memory_access_client_secret: Option<String>,
    /// Byte budget for the injected preamble. Beyond a few kilobytes the
    /// cost is real and the marginal fact is noise.
    #[arg(long, env = "PUKU_MEMORY_PREAMBLE_BYTES", default_value_t = 4096)]
    memory_preamble_bytes: usize,
    /// Skill registry (puku-skills-service). Unset means sessions get no
    /// skills, which is the pre-registry behaviour.
    #[arg(long, env = "PUKU_SKILLS_URL")]
    skills_url: Option<String>,
    /// Service credential for the skill registry, used when a session has
    /// no caller bearer to borrow: scheduled runs, `pkc_` API keys, and
    /// dev deployments with auth off.
    #[arg(long, env = "PUKU_SKILLS_TOKEN")]
    skills_token: Option<String>,
    /// MCP connector broker. The ecosystem default is the proxy puku-cowork
    /// already uses; set to an empty string to disable connectors entirely.
    #[arg(long, env = "PUKU_MCP_PROXY_URL", default_value = "https://mcp.proxy.puku.sh")]
    mcp_proxy_url: String,
    /// 32-byte hex key (openssl rand -hex 32) encrypting credentials at
    /// rest. Without it a caller's own credential cannot be stored, so no
    /// session has one and every session is refused at dispatch.
    #[arg(long, env = "PUKU_SECRET_KEY")]
    secret_key: Option<String>,
    /// Require API keys on every request ("required") or run open ("off").
    #[arg(long, env = "PUKU_AUTH", default_value = "off")]
    auth: String,
    /// The most privilege any session on this deployment may hold. Callers
    /// can ask for less, never more. `bypassPermissions` is the right
    /// default for a single-tenant box — the microVM is the boundary, and a
    /// per-tool gate inside an already-sandboxed VM mostly blocks legitimate
    /// work. Set `default` (or `plan`) on a multi-tenant deployment.
    #[arg(long, env = "PUKU_PERMISSION_CEILING", default_value = "bypassPermissions")]
    permission_ceiling: String,
    /// `--max-turns` for sessions that don't ask for one. puku-cli's own
    /// default yields after a single model response, which for an unattended
    /// session reads as "the agent did nothing".
    #[arg(long, env = "PUKU_DEFAULT_MAX_TURNS", default_value_t = 50)]
    default_max_turns: u32,
    /// Tools every session refuses unless it names a policy of its own.
    /// Comma-separated; empty (the default) restricts nothing.
    ///
    /// This is the only fleet-wide tool knob. Without it the WebSearch
    /// workaround has to be repeated by every caller on every run, and the
    /// webhook-trigger path cannot express it at all -- it builds its
    /// sessions with empty lists and no request to inherit from.
    ///
    /// A default, not a ceiling: a caller who names any tool keeps full
    /// control, including naming fewer tools than this.
    #[arg(long, env = "PUKU_DEFAULT_DISALLOWED_TOOLS", default_value = "")]
    default_disallowed_tools: String,
    /// The mirror of the above: tools a session is limited to when it names
    /// none. Empty (the default) means no allowlist.
    #[arg(long, env = "PUKU_DEFAULT_ALLOWED_TOOLS", default_value = "")]
    default_allowed_tools: String,
    /// S3-compatible endpoint for blobs and archives (Cloudflare R2 in
    /// production). Object storage is optional: without it, oversized event
    /// lines are dropped rather than dangling, and archives stay on disk.
    #[arg(long, env = "PUKU_R2_ENDPOINT")]
    r2_endpoint: Option<String>,
    #[arg(long, env = "PUKU_R2_BUCKET")]
    r2_bucket: Option<String>,
    /// R2 ignores the region but SigV4 requires one; "auto" is R2's value.
    #[arg(long, env = "PUKU_R2_REGION", default_value = "auto")]
    r2_region: String,
    #[arg(long, env = "PUKU_R2_ACCESS_KEY_ID")]
    r2_access_key_id: Option<String>,
    #[arg(long, env = "PUKU_R2_SECRET_ACCESS_KEY")]
    r2_secret_access_key: Option<String>,
    /// Engine for a session that does not name one. Must be one of
    /// PUKU_ENGINES_ALLOWED.
    #[arg(long, env = "PUKU_ENGINE_DEFAULT", default_value = "libkrun")]
    engine_default: String,
    /// Engines a caller may ask for, comma-separated: `libkrun`,
    /// `cloud_hypervisor`. A request naming anything else is refused with a
    /// 400 rather than queued for a worker that will never come.
    #[arg(long, env = "PUKU_ENGINES_ALLOWED", default_value = "libkrun")]
    engines_allowed: String,
    /// Image for a machine that names none (docs/MACHINES-API.md).
    #[arg(long, env = "PUKU_MACHINE_IMAGE", default_value = "pukubot-computer:latest")]
    machine_image: String,
    /// Ceilings a machine request is clamped to.
    #[arg(long, env = "PUKU_MACHINE_MAX_CPUS", default_value_t = 4)]
    machine_max_cpus: u8,
    #[arg(long, env = "PUKU_MACHINE_MAX_MEMORY_MIB", default_value_t = 8192)]
    machine_max_memory_mib: u32,
    /// Public base URL capability links are minted under. A separate
    /// hostname in production (routed to this same process), so guest HTML
    /// never shares an origin with the dashboard. Defaults to the listen
    /// address, which is only right for development.
    #[arg(long, env = "PUKU_LINKS_URL")]
    links_url: Option<String>,
    /// HMAC key for capability links. Defaults to one derived from
    /// PUKU_SECRET_KEY; without either, a random key per process, so links
    /// stop working when controld restarts.
    #[arg(long, env = "PUKU_LINKS_SECRET")]
    links_secret: Option<String>,
    /// How often machines are checked against their own idle timeout.
    #[arg(long, env = "PUKU_MACHINE_IDLE_SWEEP_S", default_value_t = 30)]
    machine_idle_sweep_s: u64,
    /// Machine snapshots (docs/MACHINES-API.md, "Snapshots"). `auto` offers
    /// them whenever object storage (PUKU_R2_*) and PUKU_SECRET_KEY are both
    /// set; `true` refuses to start without them; `false` never offers them.
    #[arg(long, env = "PUKU_SNAPSHOTS", default_value = "auto")]
    snapshots: String,
    /// Multipart part size for snapshot uploads, in MiB. Every part but the
    /// last is exactly this big, which R2 requires; 5 is S3's floor.
    #[arg(long, env = "PUKU_SNAPSHOT_PART_MIB", default_value_t = 64)]
    snapshot_part_mib: u64,
    /// Ready snapshots kept per machine that does not set its own `keep`.
    #[arg(long, env = "PUKU_SNAPSHOT_KEEP", default_value_t = 5)]
    snapshot_keep: u32,
    /// Days a destroyed machine's snapshots are kept before they are deleted.
    #[arg(long, env = "PUKU_SNAPSHOT_RETAIN_DESTROYED_DAYS", default_value_t = 7)]
    snapshot_retain_destroyed_days: i64,
    /// How often periodic snapshots, retention and deletion run.
    #[arg(long, env = "PUKU_SNAPSHOT_SWEEP_S", default_value_t = 60)]
    snapshot_sweep_s: u64,
    /// Directory for archived session event logs.
    #[arg(long, env = "PUKU_ARCHIVE_DIR", default_value = "./puku-archive")]
    archive_dir: PathBuf,
    /// Days after session end before events are archived out of Postgres.
    #[arg(long, env = "PUKU_RETENTION_DAYS", default_value_t = 14)]
    retention_days: i64,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the control plane (default).
    Serve,
    /// Mint a registration token for one worker host and print it once.
    GenWorkerToken {
        /// Host this token is for, e.g. the worker's --worker-name.
        #[arg(long)]
        name: String,
    },
    /// Create an org (if needed) and print a new API key for it.
    GenKey {
        #[arg(long, default_value = "dev")]
        org: String,
        /// Grant the admin scope (worker drain, fleet endpoints).
        #[arg(long)]
        admin: bool,
        #[arg(long, default_value = "")]
        name: String,
    },
}

pub struct Config {
    /// Byte budget for the injected memory preamble.
    pub memory_preamble_bytes: usize,
    pub worker_token: String,
    pub allow_shared_worker_token: bool,
    pub agent_image: String,
    pub puku_api_key: Option<String>,
    pub puku_oauth_token: Option<String>,
    pub puku_session_file: Option<std::path::PathBuf>,
    pub git_token: Option<String>,
    /// When false (the default), no operator-wide credential ever reaches a
    /// guest and a session without one of its own is refused.
    pub allow_operator_credentials: bool,
    /// OAuth issuer used to mint short-lived bearers from refresh tokens.
    pub auth_issuer: String,
    /// Model gateway the guests route through (`api-cli.puku.sh` in the
    /// ecosystem; the platform API base by default).
    pub api_url: String,
    pub auth_required: bool,
    /// Ceiling every session's permission mode is clamped to.
    pub permission_ceiling: puku_cloud_proto::session::PermissionMode,
    pub default_max_turns: u32,
    /// Applied in `build_spec` when a session names no tools of its own.
    pub default_disallowed_tools: Vec<String>,
    pub default_allowed_tools: Vec<String>,
    /// Distinguishes this controld instance in cross-instance NOTIFY fanout.
    pub instance_id: Uuid,
    /// Dev-seed identities used when auth is off.
    pub dev_org: Uuid,
    pub dev_user: Uuid,
    /// Engine a request gets when it names none.
    pub engine_default: puku_cloud_proto::Engine,
    /// Engines a request may name. Always contains `engine_default`.
    pub engines_allowed: Vec<puku_cloud_proto::Engine>,
    pub machine_image: String,
    pub machine_max_cpus: u8,
    pub machine_max_memory_mib: u32,
    /// Base URL capability links are minted under.
    pub links_base: String,
    pub machine_idle_sweep_s: u64,
    /// `None` when this deployment does not offer snapshots.
    pub snapshots: Option<snapshots::SnapshotConfig>,
}
/// Parse a comma-separated tool list from the environment.
///
/// Tolerant of the spacings an operator actually writes -- `A, B`, a trailing
/// comma, an all-whitespace value -- because the alternative is a tool named
/// `" WebFetch"` that silently matches nothing.
fn split_tools(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl Config {
    /// An operator-wide credential, or None on a shared deployment.
    ///
    /// Returning None here is what makes the dispatcher refuse a session
    /// that has no credential of its own, instead of quietly running it on
    /// the operator's account and bill.
    pub fn operator_credential(&self, value: &Option<String>) -> Option<String> {
        self.allow_operator_credentials.then(|| value.clone()).flatten()
    }

    /// The engine a request runs on, or the reason it may not.
    ///
    /// Refused at the edge, like the permission clamp: a session for an
    /// engine this deployment does not offer would otherwise sit `created`
    /// for ever waiting on a worker nobody is going to start.
    pub fn resolve_engine(
        &self,
        requested: Option<puku_cloud_proto::Engine>,
    ) -> Result<puku_cloud_proto::Engine, String> {
        let engine = requested.unwrap_or(self.engine_default);
        if self.engines_allowed.contains(&engine) {
            return Ok(engine);
        }
        let allowed: Vec<&str> = self.engines_allowed.iter().map(|e| e.as_str()).collect();
        Err(match engine {
            puku_cloud_proto::Engine::Unsupported => format!(
                "unknown engine; this deployment offers: {}",
                allowed.join(", ")
            ),
            e => format!(
                "the {e} engine is not enabled on this deployment (PUKU_ENGINES_ALLOWED); \
                 it offers: {}",
                allowed.join(", ")
            ),
        })
    }
}

/// Parse and cross-check the two engine settings. A default outside the
/// allowed set would refuse every request that names no engine, so it is a
/// boot failure rather than a runtime one.
fn engine_config(
    default: &str,
    allowed: &str,
) -> anyhow::Result<(puku_cloud_proto::Engine, Vec<puku_cloud_proto::Engine>)> {
    let allowed = puku_cloud_proto::Engine::parse_list(allowed)
        .map_err(|e| anyhow::anyhow!("PUKU_ENGINES_ALLOWED: {e}"))?;
    let default = puku_cloud_proto::Engine::parse(default).with_context(|| {
        format!("PUKU_ENGINE_DEFAULT must be libkrun or cloud_hypervisor, got {default:?}")
    })?;
    if !allowed.contains(&default) {
        anyhow::bail!(
            "PUKU_ENGINE_DEFAULT={} is not in PUKU_ENGINES_ALLOWED; every request that names \
             no engine would be refused",
            default
        );
    }
    Ok((default, allowed))
}

/// Snapshots are offered when there is somewhere to put them and a key to
/// seal their data keys with. Asking for them explicitly without either is
/// a boot failure rather than a quiet no.
fn snapshot_config(
    mode: &str,
    part_mib: u64,
    keep: u32,
    retain_destroyed_days: i64,
    sweep_s: u64,
    possible: bool,
) -> anyhow::Result<Option<snapshots::SnapshotConfig>> {
    let wanted = match mode.trim().to_ascii_lowercase().as_str() {
        "auto" | "" => None,
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => anyhow::bail!("PUKU_SNAPSHOTS must be auto, true or false, got {other:?}"),
    };
    match (wanted, possible) {
        (Some(false), _) => return Ok(None),
        (Some(true), false) => {
            anyhow::bail!("PUKU_SNAPSHOTS=true needs object storage (PUKU_R2_*) and PUKU_SECRET_KEY")
        }
        (None, false) => {
            tracing::info!("machine snapshots off: they need object storage (PUKU_R2_*) and PUKU_SECRET_KEY");
            return Ok(None);
        }
        _ => {}
    }
    let part_mib = part_mib.max(5);
    tracing::info!(part_mib, keep, "machine snapshots on");
    Ok(Some(snapshots::SnapshotConfig {
        part_bytes: part_mib << 20,
        keep: keep.max(1),
        retain_destroyed_days: retain_destroyed_days.max(0),
        sweep_s: sweep_s.max(5),
    }))
}


#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    /// Brokered connectors; None when PUKU_MCP_PROXY_URL is explicitly off.
    pub connectors: Option<Arc<connectors::ConnectorClient>>,
    /// Skill registry; None when PUKU_SKILLS_URL is unset.
    pub skills: Option<Arc<skills::SkillsClient>>,
    /// Memory service; None when PUKU_MEMORY_URL is unset.
    pub memory: Option<Arc<memory::MemoryClient>>,
    /// Platform identity verifier; None when PUKU_PLATFORM_AUTH is off.
    pub platform: Option<Arc<auth::platform::PlatformAuth>>,
    /// Encrypts credentials at rest; None when PUKU_SECRET_KEY is unset.
    pub secrets: Option<Arc<secretbox::SecretBox>>,
    /// None when the deployment has no object storage configured.
    pub blobs: Option<Arc<blobstore::BlobStore>>,
    pub hub: relay::SessionHub,
    pub workers: workerlink::WorkerRegistry,
    pub cfg: Arc<Config>,
    pub github: Arc<Option<githubapp::GithubApp>>,
    /// Idle worker data sockets, per worker.
    pub data: Arc<datalink::DataPool>,
    /// Signs and checks capability links.
    pub links: Arc<links::LinkSigner>,
    /// Ceph access for sessions on shared disks; None when PUKU_RBD_POOL
    /// is unset (those sessions then never move off a host).
    pub shared_volumes: Option<Arc<sharedvol::SharedVolumes>>,
}

/// The capability-link key: explicit, else derived from the at-rest key,
/// else random -- which works, but only until this process restarts.
fn links_signer(explicit: Option<&str>, secret_key: Option<&str>) -> links::LinkSigner {
    if let Some(s) = explicit.filter(|s| !s.trim().is_empty()) {
        return links::LinkSigner::new(s.trim().as_bytes());
    }
    if let Some(k) = secret_key.filter(|s| !s.trim().is_empty()) {
        return links::LinkSigner::new(format!("derived:{}", k.trim()).as_bytes());
    }
    tracing::warn!(
        "neither PUKU_LINKS_SECRET nor PUKU_SECRET_KEY is set: capability links use a \
         per-process key and stop working when controld restarts"
    );
    use rand::RngCore;
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    links::LinkSigner::new(&k)
}

/// True when this process is running inside a container. Docker leaves this
/// marker; the cgroup fallback covers runtimes that do not.
fn in_container() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|c| c.contains("docker") || c.contains("kubepods") || c.contains("containerd"))
            .unwrap_or(false)
}


/// What to try next, given the address that just failed.
///
/// Kept out of the log call so the advice can be tested: sending an
/// operator to the wrong side of the network costs more than saying
/// nothing, and every wrong answer here looks equally authoritative.
fn skills_url_hint(url: &str, containerised: bool) -> &'static str {
    if containerised && (url.contains("127.0.0.1") || url.contains("localhost")) {
        "loopback inside a container is this container, not the host. Publish the skills \
         service where a container can reach it (BIND_ADDR=172.17.0.1 in its .env, then \
         PUKU_SKILLS_URL=http://172.17.0.1:7870), or put both stacks on one docker \
         network and use http://skills:7870"
    } else if url.contains("172.17.0.1") {
        "the docker0 bridge answers only if the skills service is published on it. \
         BIND_ADDR=127.0.0.1 in puku-skills-service/deploy/bm/.env publishes on the host \
         loopback, which no container can reach -- set BIND_ADDR=172.17.0.1 there and \
         restart that stack"
    } else {
        "check the skills stack is running and that this address resolves and connects \
         from inside this container"
    }
}

/// Deliberately not `#[tokio::main]`.
///
/// Sentry must start before the runtime: the panic hook is then installed
/// before any task can exist, a failure during startup is still captured,
/// and the init guard's blocking flush on drop happens here rather than on a
/// runtime worker. Sentry's transport runs on its own thread with its own
/// current-thread runtime, so it never touches this one.
fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // A real binding. `let _ = ..` drops the guard immediately, disposes the
    // client and silently discards every event -- and `#[must_use]` does not
    // warn on `let _`.
    let _guard = puku_observability::init("puku-controld", sampling::traces_sampler);

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(run(args));
    // Stop producing events before the guard flushes what is queued.
    drop(rt);
    result
}

async fn run(args: Args) -> anyhow::Result<()> {

    let pool = PgPoolOptions::new()
        .max_connections(args.database_max_connections)
        .connect(&args.database_url)
        .await
        .context("connecting to postgres")?;
    sqlx::migrate!("../../migrations").run(&pool).await?;

    if let Some(Cmd::GenKey { org, admin, name }) = &args.cmd {
        return gen_key(&pool, org, *admin, name).await;
    }

    if let Some(Cmd::GenWorkerToken { name }) = &args.cmd {
        let token = workertoken::create(&pool, name).await?;
        println!("{token}");
        eprintln!(
            "worker token for {name} — store it at /etc/puku/worker-token on that host. \
             It is not recoverable; mint a new one if lost."
        );
        return Ok(());
    }

    let worker_token = match &args.worker_token_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading worker token file {path}"))?
            .trim()
            .to_string(),
        None => args.worker_token.clone(),
    };

    let auth_required = match args.auth.as_str() {
        "required" => true,
        "off" => false,
        other => anyhow::bail!("PUKU_AUTH must be 'required' or 'off', got {other:?}"),
    };

    // Fail to boot on a typo rather than silently falling back to a wider
    // policy than the operator asked for.
    let permission_ceiling = puku_cloud_proto::session::PermissionMode::parse(
        &args.permission_ceiling,
    )
    .with_context(|| {
        format!(
            "PUKU_PERMISSION_CEILING must be one of default|plan|acceptEdits|dontAsk|auto|\
             bypassPermissions, got {:?}",
            args.permission_ceiling
        )
    })?;

    let (engine_default, engines_allowed) =
        engine_config(&args.engine_default, &args.engines_allowed)?;
    tracing::info!(
        default = %engine_default,
        allowed = ?engines_allowed.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
        "engines"
    );

    // PUKU_ANTHROPIC_API_KEY predates the discovery that puku-cli targets
    // the puku platform, not Anthropic. Honour it so an existing deployment
    // doesn't silently lose its credential on upgrade.
    let puku_api_key = match (args.puku_api_key, args.legacy_anthropic_api_key) {
        (Some(key), _) => Some(key),
        (None, Some(legacy)) => {
            tracing::warn!(
                "PUKU_ANTHROPIC_API_KEY is deprecated; rename it to PUKU_AI_API_KEY"
            );
            Some(legacy)
        }
        (None, None) => None,
    };

    let blobs = blobstore::BlobStore::from_env(
        args.r2_endpoint.as_deref(),
        args.r2_bucket.as_deref(),
        &args.r2_region,
        args.r2_access_key_id.as_deref(),
        args.r2_secret_access_key.as_deref(),
    )?
    .map(Arc::new);
    // Presigned URLs are handed to the worker and fetched over whatever
    // scheme the endpoint uses. On one box that is loopback-ish and fine; the
    // moment a worker runs anywhere else, pack and artifact bytes -- and the
    // digests that authenticate them -- cross the network in the clear, and a
    // skill pack is text the model will follow.
    if let Some(ep) = args.r2_endpoint.as_deref() {
        if ep.starts_with("http://") {
            tracing::warn!(
                endpoint = %ep,
                "object storage endpoint is plain HTTP: presigned pack and artifact \
                 URLs will be fetched unencrypted. Safe only while every worker is \
                 on this host; put it behind TLS before adding a remote worker"
            );
        }
    }
    match &blobs {
        Some(b) => tracing::info!(store = ?b, "object storage configured"),
        None => tracing::warn!(
            "no object storage configured (PUKU_R2_*): oversized event lines will be \
             dropped instead of spilled, and archives stay on local disk"
        ),
    }

    let platform = args
        .platform_auth
        .then(|| Arc::new(auth::platform::PlatformAuth::new(args.api_url.clone())));
    let secrets = secretbox::SecretBox::from_env(args.secret_key.as_deref())?.map(Arc::new);
    let snapshots = snapshot_config(
        &args.snapshots,
        args.snapshot_part_mib,
        args.snapshot_keep,
        args.snapshot_retain_destroyed_days,
        args.snapshot_sweep_s,
        blobs.is_some() && secrets.is_some(),
    )?;
    if platform.is_some() {
        tracing::info!(api_url = %args.api_url, "platform auth enabled");
        if secrets.is_none() {
            tracing::warn!(
                "PUKU_SECRET_KEY is unset: a caller's own credential cannot be \
                 stored, so no session has one and every session is refused at \
                 dispatch (or, with --allow-operator-credentials, billed to the \
                 operator)"
            );
        }
    }

    // Half a token is a configuration error that would 403 every call, so it
    // is refused here rather than sent.
    let memory_access = match (
        &args.memory_access_client_id,
        &args.memory_access_client_secret,
    ) {
        (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => {
            Some(memory::AccessToken {
                client_id: id.clone(),
                client_secret: secret.clone(),
            })
        }
        (Some(_), None) | (None, Some(_)) => {
            tracing::warn!(
                "only one half of the memory Access service token is set; ignoring both"
            );
            None
        }
        _ => None,
    };

    let memory = match (&args.memory_url, &args.memory_service_key) {
        (Some(url), Some(key)) if !url.trim().is_empty() => {
            tracing::info!(
                %url,
                access = memory_access.is_some(),
                "memory service configured"
            );
            Some(Arc::new(memory::MemoryClient::new(
                url.clone(),
                key.clone(),
                memory_access,
            )))
        }
        (Some(url), None) if !url.trim().is_empty() => {
            // Better to say so than to fail every preamble with a 401 that
            // looks like the memory service being broken.
            tracing::warn!(%url, "PUKU_MEMORY_URL is set but PUKU_MEMORY_SERVICE_KEY is not; memory disabled");
            None
        }
        _ => None,
    };

    let connectors = (!args.mcp_proxy_url.trim().is_empty())
        .then(|| Arc::new(connectors::ConnectorClient::new(args.mcp_proxy_url.clone())));

    let skills = args
        .skills_url
        .as_ref()
        .filter(|u| !u.trim().is_empty())
        .map(|u| Arc::new(skills::SkillsClient::new(u.clone(), args.skills_token.clone())));
    match &skills {
        Some(client) => {
            // Ask the registry once, while somebody is still reading the
            // logs. A wrong address is otherwise invisible until dispatch,
            // and only *loud* there for a session that named packs
            // explicitly: one that named none silently loses the org's
            // defaults and improvises. /health needs no credential, so a
            // failure here is the address, not the token.
            let u = args.skills_url.clone().unwrap_or_default();
            match client.health().await {
                Ok(()) => tracing::info!(url = %u, "skill registry reachable"),
                Err(e) => tracing::error!(
                    url = %u,
                    error = %e,
                    hint = %skills_url_hint(&u, in_container()),
                    "skill registry unreachable; sessions will run WITHOUT their skill packs"
                ),
            }
        }
        None => tracing::info!("no skill registry (PUKU_SKILLS_URL unset); sessions get no skills"),
    }

    let instance_id = Uuid::new_v4();
    let shared_volumes = args.rbd_pool.clone().filter(|p| !p.trim().is_empty()).map(|rbd_pool| {
        let mut cfg = puku_volume::RbdBackendConfig::new(rbd_pool.clone(), rbd_pool.clone())
            .with_ceph_user(args.ceph_user.clone());
        if let Some(conf) = &args.ceph_conf {
            cfg = cfg.with_ceph_config(conf.clone());
        }
        let volume: Arc<dyn puku_volume::VolumeBackend> = Arc::new(puku_volume::RbdBackend::new(cfg));
        let audit = Arc::new(fence::PgAuditSink { pool: pool.clone() });
        tracing::info!(pool = %rbd_pool, "shared session disks on; sessions move hosts after fencing");
        Arc::new(sharedvol::SharedVolumes {
            pool: rbd_pool,
            fence: fence::build_fencer(volume, audit, &instance_id.to_string()),
        })
    });
    let state = AppState {
        pool,
        connectors,
        skills,
        memory,
        platform,
        secrets,
        blobs,
        hub: relay::SessionHub::new(),
        workers: workerlink::WorkerRegistry::new(),
        cfg: Arc::new(Config {
            memory_preamble_bytes: args.memory_preamble_bytes,
            worker_token,
            allow_shared_worker_token: args.allow_shared_worker_token,
            agent_image: args.agent_image,
            puku_api_key,
            puku_oauth_token: args.puku_oauth_token,
            puku_session_file: args.puku_session_file,
            git_token: args.git_token,
            allow_operator_credentials: args.allow_operator_credentials,
            auth_issuer: args.auth_issuer.clone(),
            api_url: args.puku_ai_base_url.clone().unwrap_or_else(|| args.api_url.clone()),
            auth_required,
            permission_ceiling,
            default_max_turns: args.default_max_turns,
            default_disallowed_tools: split_tools(&args.default_disallowed_tools),
            default_allowed_tools: split_tools(&args.default_allowed_tools),
            instance_id,
            dev_org: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            dev_user: Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            engine_default,
            engines_allowed,
            machine_image: args.machine_image.clone(),
            machine_max_cpus: args.machine_max_cpus.max(1),
            machine_max_memory_mib: args.machine_max_memory_mib.max(512),
            links_base: args
                .links_url
                .clone()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| format!("http://{}", args.listen_addr)),
            machine_idle_sweep_s: args.machine_idle_sweep_s,
            snapshots,
        }),
        github: Arc::new(githubapp::GithubApp::from_env()),
        data: datalink::DataPool::new(),
        links: Arc::new(links_signer(args.links_secret.as_deref(), args.secret_key.as_deref())),
        shared_volumes,
    };

    // Retry loop for sessions that couldn't be dispatched at creation time
    // (no worker online yet).
    {
        let st = state.clone();
        // Supervised: a panic here used to stop dispatch for ever while the
        // process kept answering /health, so sessions simply queued and
        // nothing said why.
        puku_observability::supervise("dispatch_pending", move || {
            let st = st.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if let Err(e) = api::dispatch_pending(&st).await {
                        tracing::warn!(error = %e, "dispatch_pending failed");
                    }
                }
            }
        });
    }

    // Housekeeping: bounded-growth chores that nothing else drives. Hourly
    // is plenty — none of these are correctness-critical, they just stop a
    // long-lived process from accumulating forever.
    {
        let st = state.clone();
        puku_observability::supervise("housekeeping", move || {
            let st = st.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    if let Some(platform) = &st.platform {
                        platform.prune_cache();
                    }
                    if let Err(e) = notify::prune(&st.pool).await {
                        tracing::warn!(error = %e, "pruning notification deliveries failed");
                    }
                }
            }
        });
    }

    archive::spawn(state.clone(), args.archive_dir.clone(), args.retention_days);
    relay::spawn_notify_listener(state.clone(), args.database_url.clone());
    scheduler::spawn(state.clone());
    api::machines::spawn_idle_sweep(state.clone());
    snapshots::spawn_sweep(state.clone());
    sweeper::spawn_all(state.clone());

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;
    tracing::info!(addr = %args.listen_addr, auth = %args.auth, "puku-controld listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn gen_key(pool: &sqlx::PgPool, org_name: &str, admin: bool, name: &str) -> anyhow::Result<()> {
    let org_id: Uuid = match sqlx::query_as::<_, (Uuid,)>("SELECT id FROM orgs WHERE name = $1")
        .bind(org_name)
        .fetch_optional(pool)
        .await?
    {
        Some((id,)) => id,
        None => {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
                .bind(id)
                .bind(org_name)
                .execute(pool)
                .await?;
            sqlx::query("INSERT INTO quotas (org_id) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(id)
                .execute(pool)
                .await?;
            id
        }
    };

    let key = auth::generate_key();
    let scopes: Vec<String> = if admin { vec!["admin".into()] } else { vec![] };
    sqlx::query(
        "INSERT INTO api_keys (id, org_id, key_hash, prefix, name, scopes) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(org_id)
    .bind(auth::hash_key(&key))
    .bind(&key[..12.min(key.len())])
    .bind(name)
    .bind(&scopes)
    .execute(pool)
    .await?;

    db::audit(pool, Some(org_id), None, "api_key.create", &key[..12], serde_json::json!({"admin": admin}))
        .await
        .ok();

    println!("{key}");
    eprintln!("org: {org_name} ({org_id})  admin: {admin} — shown once, store it now");
    Ok(())
}

#[cfg(test)]
mod skills_url_hint_tests {
    use super::skills_url_hint;

    /// The configuration that broke this deployment: controld containerised
    /// while PUKU_SKILLS_URL still said loopback.
    #[test]
    fn loopback_in_a_container_is_named_as_the_cause() {
        let h = skills_url_hint("http://127.0.0.1:7870", true);
        assert!(h.contains("this container"), "{h}");
        assert!(h.contains("172.17.0.1"), "{h}");
    }

    /// Same address, no container: loopback is then correct, so the advice
    /// must not send the operator changing a working BIND_ADDR.
    #[test]
    fn loopback_outside_a_container_gets_the_generic_hint() {
        let h = skills_url_hint("http://127.0.0.1:7870", false);
        assert!(!h.contains("this container is"), "{h}");
        assert!(h.contains("running"), "{h}");
    }

    /// The bridge address is right in shape and still fails when the skills
    /// stack publishes on host loopback -- the failure that produced no
    /// warning at all before, because it is not loopback.
    #[test]
    fn the_bridge_address_points_at_the_other_stacks_bind_addr() {
        let h = skills_url_hint("http://172.17.0.1:7870", true);
        assert!(h.contains("BIND_ADDR"), "{h}");
    }

    #[test]
    fn a_public_hostname_gets_neither_container_hint() {
        let h = skills_url_hint("https://skills.puku.sh", true);
        assert!(!h.contains("BIND_ADDR"), "{h}");
    }
}

#[cfg(test)]
mod engine_config_tests {
    use super::engine_config;
    use puku_cloud_proto::Engine;

    #[test]
    fn the_defaults_are_libkrun_only() {
        let (d, a) = engine_config("libkrun", "libkrun").unwrap();
        assert_eq!(d, Engine::Libkrun);
        assert_eq!(a, vec![Engine::Libkrun]);
    }

    /// Otherwise every request that names no engine is refused.
    #[test]
    fn a_default_outside_the_allowed_set_refuses_to_boot() {
        let err = engine_config("cloud_hypervisor", "libkrun").unwrap_err().to_string();
        assert!(err.contains("PUKU_ENGINE_DEFAULT"), "{err}");
    }

    #[test]
    fn typos_refuse_to_boot() {
        assert!(engine_config("libkrn", "libkrun").is_err());
        assert!(engine_config("libkrun", "libkrun,chv").is_err());
    }

    #[test]
    fn both_engines_can_be_offered() {
        let (d, a) = engine_config("libkrun", "libkrun, cloud_hypervisor").unwrap();
        assert_eq!(d, Engine::Libkrun);
        assert_eq!(a, vec![Engine::Libkrun, Engine::CloudHypervisor]);
    }
}

#[cfg(test)]
mod tool_list_parsing_tests {
    use super::split_tools;

    /// The spellings an operator actually writes in a .env file. A stray
    /// space would produce a tool named " WebFetch" that matches nothing --
    /// the same class of bug as the egress allowlist's whitespace test.
    #[test]
    fn tolerates_real_world_spacing() {
        assert_eq!(split_tools("WebSearch, WebFetch"), vec!["WebSearch", "WebFetch"]);
        assert_eq!(split_tools("WebSearch,WebFetch,"), vec!["WebSearch", "WebFetch"]);
        assert_eq!(split_tools("  WebSearch  "), vec!["WebSearch"]);
    }

    /// Unset and whitespace-only must both mean "no policy", not a list
    /// containing one empty string.
    #[test]
    fn empty_means_empty() {
        assert!(split_tools("").is_empty());
        assert!(split_tools("   ").is_empty());
        assert!(split_tools(",,").is_empty());
    }
}
