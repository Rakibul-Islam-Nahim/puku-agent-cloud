use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Session lifecycle state. Mirrors the CHECK constraint on `sessions.state`.
///
/// ```text
/// created -> scheduled -> booting -> bootstrapping -> running <-> waiting_input
/// running/waiting_input -> stopping -> stopped (resumable)
/// running/waiting_input -> completed | failed | canceled
/// any terminal -> reaped
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Scheduled,
    Booting,
    Bootstrapping,
    Running,
    WaitingInput,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Canceled,
    Reaped,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Created => "created",
            SessionState::Scheduled => "scheduled",
            SessionState::Booting => "booting",
            SessionState::Bootstrapping => "bootstrapping",
            SessionState::Running => "running",
            SessionState::WaitingInput => "waiting_input",
            SessionState::Stopping => "stopping",
            SessionState::Stopped => "stopped",
            SessionState::Completed => "completed",
            SessionState::Failed => "failed",
            SessionState::Canceled => "canceled",
            SessionState::Reaped => "reaped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "created" => SessionState::Created,
            "scheduled" => SessionState::Scheduled,
            "booting" => SessionState::Booting,
            "bootstrapping" => SessionState::Bootstrapping,
            "running" => SessionState::Running,
            "waiting_input" => SessionState::WaitingInput,
            "stopping" => SessionState::Stopping,
            "stopped" => SessionState::Stopped,
            "completed" => SessionState::Completed,
            "failed" => SessionState::Failed,
            "canceled" => SessionState::Canceled,
            "reaped" => SessionState::Reaped,
            _ => return None,
        })
    }

    /// Terminal states cannot transition except to `reaped`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionState::Completed
                | SessionState::Failed
                | SessionState::Canceled
                | SessionState::Reaped
        )
    }

    /// Valid state-machine edges. `reaped` is reachable from any terminal
    /// state and from `stopped` (archival of a parked session).
    pub fn can_transition_to(&self, next: SessionState) -> bool {
        use SessionState::*;
        match (self, next) {
            (Created, Scheduled) | (Created, Canceled) => true,
            (Scheduled, Booting) | (Scheduled, Failed) | (Scheduled, Canceled) => true,
            (Booting, Bootstrapping) | (Booting, Failed) | (Booting, Canceled) => true,
            (Bootstrapping, Running) | (Bootstrapping, Failed) | (Bootstrapping, Canceled) => true,
            (Running, WaitingInput)
            | (Running, Stopping)
            | (Running, Stopped) // worker-initiated idle park
            | (Running, Completed)
            | (Running, Failed)
            | (Running, Canceled) => true,
            (WaitingInput, Running)
            | (WaitingInput, Stopping)
            | (WaitingInput, Stopped) // worker-initiated idle park
            | (WaitingInput, Completed)
            | (WaitingInput, Failed)
            | (WaitingInput, Canceled) => true,
            (Stopping, Stopped) | (Stopping, Failed) => true,
            // Resume is a cold boot of a fresh sandbox on the same volumes,
            // which is as true of a session that finished its turn as of one
            // that was parked. Since a successful result now completes a
            // session rather than leaving it running, continuing a
            // conversation goes through here -- without these two, a
            // follow-up message is refused with
            // "invalid transition completed -> scheduled".
            (Stopped, Scheduled) | (Stopped, Reaped) | (Stopped, Canceled) => true,
            (Completed, Scheduled) | (Failed, Scheduled) => true,
            (Completed, Reaped) | (Failed, Reaped) | (Canceled, Reaped) => true,
            _ => false,
        }
    }
}

/// puku-cli's `--permission-mode` values, verified against puku-cli 1.8.43
/// (`--permission-mode <mode>`: acceptEdits, bypassPermissions, default,
/// dontAsk, plan, auto). Serialized in puku-cli's own spelling so the runner
/// can pass the value straight through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_cli_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::Plan => "plan",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Auto => "auto",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }

    /// Parse puku-cli's spelling back. Returns `None` for anything else so
    /// callers can fall back to the deployment default rather than guessing.
    pub fn parse(s: &str) -> Option<PermissionMode> {
        match s {
            "default" => Some(PermissionMode::Default),
            "plan" => Some(PermissionMode::Plan),
            "acceptEdits" => Some(PermissionMode::AcceptEdits),
            "dontAsk" => Some(PermissionMode::DontAsk),
            "auto" => Some(PermissionMode::Auto),
            "bypassPermissions" => Some(PermissionMode::BypassPermissions),
            _ => None,
        }
    }

    /// How much the mode lets the agent do without asking. Only used to
    /// compare two modes, never exposed on the wire.
    fn privilege(self) -> u8 {
        match self {
            PermissionMode::Plan => 0,
            PermissionMode::Default => 1,
            PermissionMode::AcceptEdits => 2,
            PermissionMode::DontAsk => 3,
            PermissionMode::Auto => 3,
            PermissionMode::BypassPermissions => 4,
        }
    }

    /// A caller may narrow the deployment's ceiling, never widen it.
    /// Returns the effective mode for a request of `self` under `ceiling`.
    pub fn clamp_to(self, ceiling: PermissionMode) -> PermissionMode {
        if self.privilege() > ceiling.privilege() {
            ceiling
        } else {
            self
        }
    }
}

/// One MCP server the guest should load.
///
/// Connectors are brokered: `url` points at mcp.proxy.puku.sh and the
/// Authorization header is the literal string `Bearer ${PUKU_API_KEY}`,
/// which puku-cli expands from the guest env. The vendor's OAuth token
/// stays server-side at the proxy and never enters the microVM, so a prompt
/// injection inside the VM cannot exfiltrate it. Same shape puku-cowork
/// writes for local VM sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerSpec {
    /// Key in puku-cli's `mcpServers` map, e.g. "puku.ai Slack".
    pub name: String,
    #[serde(rename = "type")]
    pub transport: String,
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl McpServerSpec {
    /// Build the entry for one brokered connector.
    pub fn connector(proxy_url: &str, id: &str, display_name: &str) -> Self {
        McpServerSpec {
            name: format!("puku.ai {display_name}"),
            transport: "http".to_string(),
            url: format!("{}/v1/mcp/{id}", proxy_url.trim_end_matches('/')),
            headers: [(
                "Authorization".to_string(),
                // Deliberately a placeholder, not the token: this value is
                // written into a file on the session volume.
                "Bearer ${PUKU_API_KEY}".to_string(),
            )]
            .into_iter()
            .collect(),
        }
    }
}

/// A transcript lifted out of a local puku-cli run, to be continued in the
/// cloud.
///
/// Two carriers because object storage is optional: a deployment with R2
/// gets a presigned URL and no size ceiling, while a dev box without it can
/// still teleport a small session inline. Anything larger than
/// `MAX_INLINE_IMPORT_BYTES` without object storage is refused with an
/// explanation rather than silently truncated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum ImportRef {
    /// Presigned GET the worker fetches the transcript from.
    Url { url: String },
    /// The transcript itself, for deployments with no object storage.
    Inline { jsonl: String },
}

/// Cap on the inline carrier. Transcripts run to tens of megabytes for long
/// sessions; putting one of those through a WebSocket control frame would
/// stall every other session on that worker.
pub const MAX_INLINE_IMPORT_BYTES: usize = 1024 * 1024;

/// A resolved skill pack: everything the worker needs to fetch and trust
/// it.
///
/// The digest is the point. The worker verifies what it downloads against
/// this before unpacking, so a swapped object in storage cannot inject
/// instructions into a session — a skill is text the model will follow, so
/// the bytes need the same care as a binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillPackRef {
    pub name: String,
    pub version: String,
    /// sha256 of the tarball, hex.
    pub digest: String,
    /// Presigned URL, minted at dispatch so it is fresh when used.
    pub url: String,
}

/// Everything a worker needs to run (or resume) a session. Sent inside
/// `Down::AssignSession`; also serialized into the in-guest bootstrap
/// manifest (minus host-only fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub session_id: Uuid,
    /// msb sandbox name, `ses-<short id>`.
    pub sandbox_name: String,
    /// Hypervisor to boot under. Defaulted so a spec from an older controld,
    /// and every `spec.json` written before engines existed, reads as
    /// libkrun -- which is what it ran on.
    #[serde(default)]
    pub engine: crate::engine::Engine,
    /// OCI image reference (by digest in production).
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub idle_timeout_s: u32,
    pub max_duration_s: u32,

    /// The user's task. Empty when `resume` is true (the transcript carries it).
    pub prompt: String,
    /// Git repo to clone (https URL without credentials), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Short-lived token for the clone/push. Never logged, shredded in-guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_token: Option<String>,

    /// puku-cli knobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// puku-cli `--permission-mode`. The caller may only narrow the
    /// deployment profile's ceiling; controld clamps before dispatch.
    /// `None` means "use the worker's profile default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// puku-cli `--max-turns`. Unset lets puku-cli decide, which in practice
    /// ends the run after a single model response — useless for an unattended
    /// cloud session, so controld always fills this in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// MCP servers to load. Guest-visible: the headers hold a placeholder,
    /// not a credential.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerSpec>,
    /// Skill packs to materialize into the guest's skills root before the
    /// agent starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillPackRef>,
    /// Prose recalled from this repository's memory profile, injected as an
    /// appended system prompt.
    ///
    /// Guest-visible, and framed by the runner as BACKGROUND rather than
    /// instruction: it is derived from earlier sessions that ran
    /// model-authored code and may have read untrusted pages.
    ///
    /// Pinned to the session at first dispatch and replayed verbatim on
    /// resume, so a parked session never wakes up with a system prompt that
    /// disagrees with its own transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_preamble: Option<String>,
    /// JSON Schema the final answer must satisfy, for runs whose output is
    /// consumed by a program rather than read by a person — a nightly job
    /// that should return a verdict, not prose.
    ///
    /// Delivered to the CLI as `--json-schema`, deliberately not through the
    /// SDK's `outputFormat`: that option also appends `--output-format json`,
    /// and the CLI refuses that combination outright
    /// ("--input-format=stream-json requires output-format=stream-json"),
    /// which would take the whole event stream with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,

    /// Puku platform API key for this session, delivered to the guest as
    /// `PUKU_AI_API_KEY`. The guest sees only a `$MSB_…` placeholder; the
    /// real value is injected at the network boundary for the puku API
    /// hosts (see workerd's `PUKU_SECRET_HOSTS`).
    ///
    /// The `anthropic_api_key` alias keeps pre-rename `spec.json` files on
    /// session volumes — and an older controld's frames — deserializable.
    #[serde(
        default,
        alias = "anthropic_api_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub puku_api_key: Option<String>,
    /// The *caller's own* puku platform bearer, delivered to the guest as
    /// `ANTHROPIC_AUTH_TOKEN` (and `PUKU_API_KEY`, which connector MCP
    /// headers expand). This is the spawnerd contract puku-cowork already
    /// uses for local VM sessions; matching it means a cloud session bills
    /// the user who started it instead of the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub puku_auth_token: Option<String>,
    /// Model gateway the guest routes through — `ANTHROPIC_BASE_URL` /
    /// `PUKU_AI_BASE_URL` / `PUKU_WORKER_URL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub puku_api_base: Option<String>,
    /// Alternative auth: a long-lived puku subscription token
    /// (`puku-cli setup-token`), delivered as PUKU_CLI_OAUTH_TOKEN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub puku_oauth_token: Option<String>,
    /// Preferred auth: the content of a puku session file
    /// (~/.config/pukucode/session.json). workerd plants it in the guest
    /// HOME; puku-cli uses and auto-refreshes it on the session volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub puku_session_json: Option<String>,

    /// True when this assignment resumes a parked session on existing volumes.
    #[serde(default)]
    pub resume: bool,
    /// Highest guest_line controld has persisted; the worker starts tailing
    /// the outbox file after this line.
    #[serde(default)]
    pub events_cursor: i64,
    /// puku-cli's own session id, required when `resume` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub puku_session_id: Option<String>,
    /// A transcript to plant before launch so `--resume` continues a
    /// conversation that started on someone's laptop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<ImportRef>,
}

/// The bootstrap manifest written to `/session/manifest.json` on the volume.
/// Guest-visible subset of `SessionSpec` — host-only fields excluded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestManifest {
    pub session_id: Uuid,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Prose recalled from this repository's memory profile, injected as an
    /// appended system prompt.
    ///
    /// Guest-visible, and framed by the runner as BACKGROUND rather than
    /// instruction: it is derived from earlier sessions that ran
    /// model-authored code and may have read untrusted pages.
    ///
    /// Pinned to the session at first dispatch and replayed verbatim on
    /// resume, so a parked session never wakes up with a system prompt that
    /// disagrees with its own transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_preamble: Option<String>,
    /// Already clamped to the deployment ceiling by controld: the guest
    /// passes it through, it never re-decides policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerSpec>,
    /// Guest-visible record of which packs are installed: name@version
    /// only. The presigned URLs stay host-side — they are credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub puku_session_id: Option<String>,
    /// Egress domain-suffix allowlist in force for this VM. Guest-visible
    /// on purpose: the in-guest proxy serves it, so a blocked request can
    /// be reported as a policy decision instead of a DNS failure.
    /// Never a credential — it is the same list msb enforces.
    #[serde(default)]
    pub egress_allow: Vec<String>,
}

impl From<&SessionSpec> for GuestManifest {
    fn from(s: &SessionSpec) -> Self {
        GuestManifest {
            session_id: s.session_id,
            prompt: s.prompt.clone(),
            repo: s.repo.clone(),
            branch: s.branch.clone(),
            git_token: s.git_token.clone(),
            model: s.model.clone(),
            max_budget_usd: s.max_budget_usd,
            allowed_tools: s.allowed_tools.clone(),
            disallowed_tools: s.disallowed_tools.clone(),
            permission_mode: s.permission_mode,
            memory_preamble: s.memory_preamble.clone(),
            max_turns: s.max_turns,
            mcp_servers: s.mcp_servers.clone(),
            skills: s
                .skills
                .iter()
                .map(|p| format!("{}@{}", p.name, p.version))
                .collect(),
            resume: s.resume,
            puku_session_id: s.puku_session_id.clone(),
            // Worker-level config; filled in by workerd after conversion.
            output_schema: s.output_schema.clone(),
            egress_allow: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_json(key_field: &str) -> String {
        format!(
            r#"{{"session_id":"00000000-0000-0000-0000-000000000001",
                 "sandbox_name":"ses-test","image":"puku-agent:latest",
                 "cpus":2,"memory_mib":2048,"idle_timeout_s":600,
                 "max_duration_s":3600,"prompt":"hi",{key_field}}}"#
        )
    }

    /// A spec.json written before the rename (or sent by an older controld)
    /// must still carry its credential into the new field, or in-flight
    /// sessions lose auth on upgrade.
    #[test]
    fn legacy_anthropic_api_key_field_deserializes() {
        let spec: SessionSpec =
            serde_json::from_str(&spec_json(r#""anthropic_api_key":"secret-value""#)).unwrap();
        assert_eq!(spec.puku_api_key.as_deref(), Some("secret-value"));
    }

    #[test]
    fn current_puku_api_key_field_deserializes() {
        let spec: SessionSpec =
            serde_json::from_str(&spec_json(r#""puku_api_key":"secret-value""#)).unwrap();
        assert_eq!(spec.puku_api_key.as_deref(), Some("secret-value"));
    }

    /// A presigned pack URL is a credential; only name@version may reach
    /// the manifest that sits on the session volume.
    #[test]
    fn guest_manifest_excludes_pack_urls() {
        let mut spec: SessionSpec =
            serde_json::from_str(&spec_json(r#""model":"opus""#)).unwrap();
        spec.skills = vec![SkillPackRef {
            name: "office".into(),
            version: "1.0.0".into(),
            digest: "abc123".into(),
            url: "https://storage.test/packs/abc123.tgz?X-Amz-Signature=secret".into(),
        }];
        let manifest = serde_json::to_string(&GuestManifest::from(&spec)).unwrap();
        assert!(manifest.contains("office@1.0.0"));
        assert!(!manifest.contains("X-Amz-Signature"), "manifest leaked a presigned url");
    }

    /// Credentials must never reach the guest-visible manifest.
    #[test]
    fn guest_manifest_excludes_credentials() {
        let spec: SessionSpec =
            serde_json::from_str(&spec_json(r#""puku_api_key":"secret-value""#)).unwrap();
        let manifest = serde_json::to_string(&GuestManifest::from(&spec)).unwrap();
        assert!(!manifest.contains("secret-value"), "manifest leaked: {manifest}");
    }

    /// The header must carry the placeholder, never a real token: the
    /// manifest it lands in is a plain file on the session volume.
    #[test]
    fn connector_entries_never_embed_a_credential() {
        let e = McpServerSpec::connector("https://mcp.proxy.puku.sh", "slack", "Slack");
        assert_eq!(e.name, "puku.ai Slack");
        assert_eq!(e.url, "https://mcp.proxy.puku.sh/v1/mcp/slack");
        assert_eq!(e.transport, "http");
        assert_eq!(e.headers["Authorization"], "Bearer ${PUKU_API_KEY}");
    }

    /// A trailing slash on the configured proxy must not produce `//v1`.
    #[test]
    fn connector_urls_survive_a_trailing_slash() {
        let e = McpServerSpec::connector("https://mcp.proxy.puku.sh/", "gmail", "Gmail");
        assert_eq!(e.url, "https://mcp.proxy.puku.sh/v1/mcp/gmail");
    }

    /// The whole point of the ceiling: a request can ask for less privilege
    /// than the deployment allows, never more.
    #[test]
    fn permission_mode_narrows_but_never_widens() {
        use PermissionMode::*;
        // A multi-tenant ceiling refuses to be widened.
        assert_eq!(BypassPermissions.clamp_to(Default), Default);
        assert_eq!(DontAsk.clamp_to(Default), Default);
        // Narrowing is always honoured.
        assert_eq!(Plan.clamp_to(BypassPermissions), Plan);
        assert_eq!(Default.clamp_to(BypassPermissions), Default);
        // Equal privilege passes through untouched.
        assert_eq!(AcceptEdits.clamp_to(AcceptEdits), AcceptEdits);
    }

    /// The runner passes this string straight to puku-cli, so the spelling
    /// is load-bearing: these are the six values `--permission-mode` accepts
    /// in puku-cli 1.8.43.
    #[test]
    fn permission_mode_uses_puku_cli_spelling() {
        use PermissionMode::*;
        for (mode, expected) in [
            (Default, "default"),
            (Plan, "plan"),
            (AcceptEdits, "acceptEdits"),
            (DontAsk, "dontAsk"),
            (Auto, "auto"),
            (BypassPermissions, "bypassPermissions"),
        ] {
            assert_eq!(mode.as_cli_str(), expected);
            // The wire format must match the CLI spelling too, so a manifest
            // can be read back without a translation table.
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{expected}\""));
        }
    }
}
