//! Worker <-> control-plane protocol.
//!
//! Workers dial OUT to controld (`WS /v1/worker`) and keep one persistent
//! WebSocket open. Every frame is one JSON text message. The connection is
//! the heartbeat, the assignment channel, and the event pipe.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::engine::Engine;
use crate::event::GuestEvent;
use crate::machine::{MachineSpec, MachineState};
use crate::session::{SessionSpec, SessionState};
use crate::snapshot::{Consistency, Fingerprint, PartDone, PartUrl, SnapshotLayer, SnapshotOrder, SnapshotProgress};

/// Feature name a worker advertises when it can run machines and serve the
/// data plane (`crate::data_proto`).
pub const FEATURE_MACHINES: &str = "machines";

/// Feature name a worker advertises when it sends `Up::LeaseRenew` every
/// second. Controld takes over the host's liveness lease on registration
/// and renews it on each frame; a worker without the feature has no lease
/// and is never declared dead by the sweeper.
pub const FEATURE_LEASE: &str = "lease";

/// What a worker's host has, for the placement decisions controld makes
/// before it assigns anything: whether a machine could *ever* fit here, and
/// whether its image is staged. Every field is optional, and a field a
/// worker leaves out is never a reason to refuse it -- that is what keeps an
/// older worker placeable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_mib: Option<u64>,
    /// The most slots this host could offer with nothing running. This is
    /// what separates "too large for this host" from "full right now", which
    /// `capacity_slots` cannot: it shrinks as the box fills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_slots: Option<u32>,
    /// Free and total space on the filesystem holding volumes and VM disks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_mib: Option<u64>,
    /// Guest images staged ahead of time, for engines that boot a prepared
    /// disk rather than pulling (Cloud Hypervisor). `None` means "not
    /// reported", never "none staged".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<StagedImage>>,
}

/// One guest image staged on a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedImage {
    pub engine: Engine,
    /// `machine::image_key` of the reference it was staged from.
    pub key: String,
    /// The reference itself, when the staging script recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Content digest (`sha256:...`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mib: Option<u64>,
}

/// Maximum events per `SessionEvents` frame (frames stay well under 64 KB
/// with the 256 KB line cap applied guest-side before batching).
pub const MAX_EVENTS_PER_FRAME: usize = 50;

/// Frames sent worker -> controld.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Up {
    Register {
        worker_name: String,
        /// Shared token for M1; per-worker tokens in M4.
        auth_token: String,
        capacity_slots: u32,
        msb_version: String,
        /// Sessions this worker still has sandboxes/state for (crash recovery).
        running_sessions: Vec<Uuid>,
        /// Every session this worker still holds a directory for.
        ///
        /// Reaping is otherwise fire-and-forget at archival time, which only
        /// lands if that exact worker happens to be connected. A worker that
        /// was restarted, renamed or offline would keep those volumes for
        /// ever. Declaring what is on disk lets the control plane answer with
        /// what is safe to delete, so a restart reconciles instead of leaking.
        #[serde(default)]
        on_disk_sessions: Vec<Uuid>,
        /// Engines this worker can boot. Empty means libkrun only: that is
        /// every worker built before engines existed, and it must never be
        /// handed a spec for anything else, because it would ignore the
        /// field and boot libkrun regardless.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        engines: Vec<Engine>,
        /// Optional capabilities beyond running sessions, by name. controld
        /// only sends a frame family to a worker that advertised it: an
        /// older worker skips frames it cannot parse, so an unadvertised
        /// feature would be a request that silently never happens.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
        /// Machines with a live VM on this worker, and every machine it
        /// still holds a volume for. The machine counterparts of
        /// `running_sessions` / `on_disk_sessions`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        running_machines: Vec<Uuid>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        on_disk_machines: Vec<Uuid>,
        /// The host's resources and staged images. Absent on older workers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<HostReport>,
    },
    Heartbeat {
        used_slots: u32,
        /// Capacity as the host sees it *now*.
        ///
        /// A fixed number set at registration cannot know that the box has
        /// filled up. Defaulted so an older worker, which sends none, keeps
        /// whatever it registered with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capacity_slots: Option<u32>,
        /// Sandbox names this host actually has, as msb reports them.
        ///
        /// The database's view and the host's view disagree in exactly the
        /// cases an operator needs to see: a session the platform thinks is
        /// running whose VM died, and an orphaned sandbox holding memory
        /// with no session behind it. Defaulted so an older worker still
        /// deserializes.
        #[serde(default)]
        sandboxes: Vec<String>,
        /// As on `Register`, refreshed: free disk and staged images change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<HostReport>,
    },
    /// "Still alive", once a second, from a worker that advertised
    /// `FEATURE_LEASE`. Kept separate from `Heartbeat` (every 10 s, carries
    /// inventory) so liveness is cheap and frequent.
    LeaseRenew,
    SessionEvents {
        session_id: Uuid,
        events: Vec<GuestEvent>,
    },
    SessionState {
        session_id: Uuid,
        state: SessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// puku-cli's own session id, reported once known (needed for resume).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        puku_session_id: Option<String>,
    },
    SessionUsage {
        session_id: Uuid,
        cost_usd: f64,
        /// Fresh (uncached) input tokens, as puku-cli reports them.
        tokens_in: i64,
        tokens_out: i64,
        /// Prompt-cache traffic, reported separately by puku-cli and billed
        /// at different rates. Defaulted so a pre-upgrade worker still
        /// deserializes.
        #[serde(default)]
        cache_read_tokens: i64,
        #[serde(default)]
        cache_write_tokens: i64,
    },
    /// The agent is blocked on user input (question, permission, plan gate).
    /// controld flips the session to waiting_input and stores the question.
    PendingQuestion {
        session_id: Uuid,
        question: serde_json::Value,
    },
    /// Ask controld for a fresh repo-scoped token to push with.
    RequestGitToken {
        session_id: Uuid,
    },
    /// A `CollectArtifact` finished (or failed).
    ArtifactReady {
        session_id: Uuid,
        what: ArtifactKind,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The runner pushed a branch and wants a PR opened for it.
    BranchPushed {
        session_id: Uuid,
        branch: String,
    },
    /// Ask for a presigned PUT so the worker can push a file to object
    /// storage. Bucket credentials live only on controld, so this
    /// round-trip is what stands between a worker and the whole store.
    RequestUpload {
        session_id: Uuid,
        /// Object key, derived by the worker so it can name the object
        /// before the upload completes.
        key: String,
        content_type: String,
    },
    /// A machine changed state on this worker. `generation` is the boot it
    /// belongs to; controld drops frames from an older boot.
    MachineState {
        machine_id: Uuid,
        generation: u64,
        state: MachineState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// On `running`: whether the volume was already on disk before this
        /// boot. False means the caller's files are not there.
        #[serde(default)]
        volume_existed: bool,
        /// On `failed`: why, as a word a client can act on
        /// (`image_not_staged`, `insufficient_disk`, ...). `error` keeps the
        /// sentence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Presigned URLs for `count` parts of a snapshot layer, from
    /// `first_part` (1-based).
    RequestSnapshotUrls {
        snapshot_id: Uuid,
        layer: SnapshotLayer,
        first_part: u32,
        count: u32,
    },
    /// Every part of a layer is uploaded; controld completes the upload.
    SnapshotLayerDone {
        snapshot_id: Uuid,
        layer: SnapshotLayer,
        parts: Vec<PartDone>,
        /// Bytes before compression and encryption.
        plain_bytes: u64,
        stored_bytes: u64,
        /// Of the stored object, hex.
        sha256: String,
    },
    /// A capture's progress. On `ready`, `reused` names layers left out
    /// because an earlier snapshot already holds them; a layer neither
    /// uploaded nor reused is absent from this snapshot.
    SnapshotState {
        snapshot_id: Uuid,
        machine_id: Uuid,
        state: SnapshotProgress,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consistency: Option<Consistency>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<Fingerprint>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reused: Vec<SnapshotLayer>,
    },
    /// A presigned GET for one layer of the snapshot this worker is
    /// restoring a machine from.
    RequestSnapshotGet {
        snapshot_id: Uuid,
        layer: SnapshotLayer,
    },
}

/// Frames sent controld -> worker.
// AssignSession carries a whole SessionSpec and dwarfs the other variants.
// Boxing it would save nothing that matters: these frames are built,
// serialized to JSON and dropped, never held in a collection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Down {
    RegisterAck {
        /// Of the worker's `on_disk_sessions`, those already archived: the
        /// transcript is safe and the rows are gone, so the volumes are dead
        /// weight. Anything not listed is left alone.
        #[serde(default)]
        reapable: Vec<Uuid>,
        worker_id: Uuid,
        /// Highest persisted guest_line per session this worker owns; the
        /// worker re-tails its outbox files from these cursors.
        resume_cursors: HashMap<Uuid, i64>,
        /// Of the worker's `on_disk_machines`, those destroyed or unknown to
        /// the control plane: their volumes can go.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reapable_machines: Vec<Uuid>,
    },
    AssignSession {
        spec: SessionSpec,
    },
    /// One puku-cli stream-json line to write to the session's stdin fifo.
    DeliverInput {
        session_id: Uuid,
        stream_json_line: String,
    },
    Interrupt {
        session_id: Uuid,
    },
    StopSession {
        session_id: Uuid,
        mode: StopMode,
    },
    /// Package part of a live session and upload it to object storage, so
    /// the caller can pull work out of the VM before the volume is reaped.
    /// The worker answers with `Up::ArtifactReady`.
    CollectArtifact {
        session_id: Uuid,
        what: ArtifactKind,
        /// Key to upload to; the worker requests a presigned PUT for it.
        key: String,
    },
    /// A freshly minted git token for the push at session end. Installation
    /// tokens expire in an hour, so the clone token is usually dead by then.
    GitToken {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Answer to `Up::RequestUpload`. `url` is absent when the deployment
    /// has no object storage configured; the worker then skips the upload
    /// instead of retrying forever.
    /// The control plane is done with this session's volumes: it has
    /// archived the transcript and dropped its rows, so nothing will ask for
    /// the workspace again.
    ///
    /// Without this the worker keeps every session's `/workspace` and its
    /// unpacked skill packs forever. `StopMode::Kill` already promised
    /// "volumes kept until reaping" -- this is the reaping.
    ReapSession {
        session_id: Uuid,
    },
    UploadUrl {
        session_id: Uuid,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Boot (or reboot) a machine. Only sent to a worker that advertised
    /// `FEATURE_MACHINES`.
    AssignMachine { spec: MachineSpec },
    /// Stop a machine's VM and keep its volume. Ignored unless `generation`
    /// is the boot the worker is running.
    StopMachine {
        machine_id: Uuid,
        generation: u64,
        /// Snapshot the disks once the VM is down. Only ever sent to a worker
        /// that advertised `FEATURE_SNAPSHOTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<SnapshotOrder>,
    },
    /// Stop the VM and delete the volume -- after a last snapshot, when one
    /// is ordered.
    DestroyMachine {
        machine_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_snapshot: Option<SnapshotOrder>,
    },
    /// The data-socket pool for this worker is empty: dial `count` more.
    OpenDataSockets { count: u32 },
    /// Capture a snapshot of a machine this worker holds: live while its VM
    /// runs, clean when it is stopped.
    SnapshotMachine { order: SnapshotOrder },
    /// Answer to `Up::RequestSnapshotUrls`; `error` instead of URLs when
    /// there are none to give.
    SnapshotUrls {
        snapshot_id: Uuid,
        layer: SnapshotLayer,
        first_part: u32,
        #[serde(default)]
        urls: Vec<PartUrl>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Answer to `Up::RequestSnapshotGet`; `None` when refused.
    SnapshotGetUrl {
        snapshot_id: Uuid,
        layer: SnapshotLayer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// The machine now lives on another worker, restored from a snapshot:
    /// drop this worker's copy of it -- unless this worker runs a boot at
    /// least this new, which would mean the copy is the live one.
    ReapMachine { machine_id: Uuid, below_generation: u64 },
}

/// What to pull out of a session's volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// `/workspace` — the code the agent wrote.
    Workspace,
    /// `/session/home` — puku-cli's own transcript, which is what a local
    /// `puku-cli --resume` needs to continue the conversation off-cloud.
    Home,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Workspace => "workspace",
            ArtifactKind::Home => "home",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(ArtifactKind::Workspace),
            "home" => Some(ArtifactKind::Home),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    /// Stop the sandbox, keep volumes: session becomes `stopped` (resumable).
    Park,
    /// Kill and mark canceled; volumes kept until reaping.
    Kill,
}

/// Frames exactly as the previous release put them on the wire. These are
/// the compatibility promise: a mixed fleet mid-upgrade sends both shapes.
#[cfg(test)]
mod compat_tests {
    use super::*;

    #[test]
    fn a_pre_engine_register_frame_still_parses() {
        let old = r#"{"type":"register","worker_name":"box-1","auth_token":"t",
            "capacity_slots":4,"msb_version":"sdk-0.1.0","running_sessions":[],
            "on_disk_sessions":[]}"#;
        match serde_json::from_str::<Up>(old).unwrap() {
            Up::Register { engines, features, .. } => {
                assert!(engines.is_empty(), "absent means libkrun-only, decided by controld");
                assert!(features.is_empty());
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn a_pre_engine_assignment_reads_as_libkrun() {
        let old = r#"{"type":"assign_session","spec":{
            "session_id":"00000000-0000-0000-0000-000000000001",
            "sandbox_name":"ses-000000000000","image":"puku-agent:latest",
            "cpus":2,"memory_mib":2048,"idle_timeout_s":600,"max_duration_s":3600,
            "prompt":"hi"}}"#;
        match serde_json::from_str::<Down>(old).unwrap() {
            Down::AssignSession { spec } => assert_eq!(spec.engine, Engine::Libkrun),
            other => panic!("wrong frame: {other:?}"),
        }
    }

    /// A libkrun-only worker's Register must look exactly like it used to,
    /// so an older controld does not see fields it has no column for.
    #[test]
    fn empty_capability_lists_stay_off_the_wire() {
        let frame = Up::Register {
            worker_name: "w".into(),
            auth_token: "t".into(),
            capacity_slots: 1,
            msb_version: "v".into(),
            running_sessions: vec![],
            on_disk_sessions: vec![],
            engines: vec![],
            features: vec![],
            running_machines: vec![],
            on_disk_machines: vec![],
            host: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(!json.contains("engines") && !json.contains("features"), "{json}");
        assert!(!json.contains("machines") && !json.contains("host"), "{json}");
    }

    #[test]
    fn lease_renew_is_a_bare_tag() {
        let json = serde_json::to_string(&Up::LeaseRenew).unwrap();
        assert_eq!(json, r#"{"type":"lease_renew"}"#);
        assert!(matches!(serde_json::from_str::<Up>(&json).unwrap(), Up::LeaseRenew));
    }

    /// Heartbeats and machine reports from before hosts reported limits.
    #[test]
    fn pre_host_report_frames_still_parse() {
        let hb = r#"{"type":"heartbeat","used_slots":1,"sandboxes":[]}"#;
        match serde_json::from_str::<Up>(hb).unwrap() {
            Up::Heartbeat { host, .. } => assert!(host.is_none()),
            other => panic!("wrong frame: {other:?}"),
        }
        let ms = r#"{"type":"machine_state","machine_id":"00000000-0000-0000-0000-000000000001",
            "generation":1,"state":"failed","error":"boom"}"#;
        match serde_json::from_str::<Up>(ms).unwrap() {
            Up::MachineState { reason, error, .. } => {
                assert!(reason.is_none());
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    /// Stop and destroy as a controld from before snapshots sent them, and
    /// as this one still does for a worker without the feature.
    #[test]
    fn stop_and_destroy_keep_their_old_shape() {
        let old = r#"{"type":"stop_machine","machine_id":"00000000-0000-0000-0000-000000000001","generation":2}"#;
        match serde_json::from_str::<Down>(old).unwrap() {
            Down::StopMachine { snapshot, generation, .. } => {
                assert!(snapshot.is_none());
                assert_eq!(generation, 2);
            }
            other => panic!("wrong frame: {other:?}"),
        }
        let frame = Down::DestroyMachine { machine_id: Uuid::nil(), final_snapshot: None };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(!json.contains("final_snapshot"), "{json}");
    }

    /// A partial report is a valid one: every limit is optional.
    #[test]
    fn a_host_report_carries_only_what_the_host_knows() {
        let json = r#"{"max_slots":11,"images":[{"engine":"cloud_hypervisor","key":"pukubot_latest"}]}"#;
        let h: HostReport = serde_json::from_str(json).unwrap();
        assert_eq!(h.max_slots, Some(11));
        assert!(h.cores.is_none() && h.disk_free_mib.is_none());
        let imgs = h.images.unwrap();
        assert_eq!(imgs[0].engine, Engine::CloudHypervisor);
        assert!(imgs[0].digest.is_none());
    }

    /// An ack from a controld that predates machines.
    #[test]
    fn a_pre_machine_register_ack_still_parses() {
        let old = r#"{"type":"register_ack","reapable":[],
            "worker_id":"00000000-0000-0000-0000-000000000001","resume_cursors":{}}"#;
        match serde_json::from_str::<Down>(old).unwrap() {
            Down::RegisterAck { reapable_machines, .. } => assert!(reapable_machines.is_empty()),
            other => panic!("wrong frame: {other:?}"),
        }
    }
}
