//! Machines: generic VMs driven from outside (docs/MACHINES-API.md).
//!
//! Where a session is puku-cli in a VM, a machine is only the VM: the caller
//! picks the image and talks to it through exec, files and ports. That is
//! the shape puku-bot's computers need, and nothing here knows about them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::Engine;

/// Machine lifecycle. Mirrors the CHECK constraint on `machines.state`.
///
/// ```text
/// scheduled -> [restoring ->] booting -> running -> stopping -> stopped
/// scheduled|restoring|booting|running -> failed
/// stopped|failed -> scheduled            (start, restore)
/// anything -> destroyed                  (terminal)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Scheduled,
    Booting,
    /// Laying a snapshot's disks down on its new worker, before booting.
    Restoring,
    Running,
    Stopping,
    Stopped,
    Failed,
    Destroyed,
}

impl MachineState {
    pub fn as_str(self) -> &'static str {
        match self {
            MachineState::Scheduled => "scheduled",
            MachineState::Booting => "booting",
            MachineState::Restoring => "restoring",
            MachineState::Running => "running",
            MachineState::Stopping => "stopping",
            MachineState::Stopped => "stopped",
            MachineState::Failed => "failed",
            MachineState::Destroyed => "destroyed",
        }
    }

    pub fn parse(s: &str) -> Option<MachineState> {
        Some(match s {
            "scheduled" => MachineState::Scheduled,
            "booting" => MachineState::Booting,
            "restoring" => MachineState::Restoring,
            "running" => MachineState::Running,
            "stopping" => MachineState::Stopping,
            "stopped" => MachineState::Stopped,
            "failed" => MachineState::Failed,
            "destroyed" => MachineState::Destroyed,
            _ => return None,
        })
    }

    /// Holding (or about to hold) a VM on a worker.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            MachineState::Scheduled
                | MachineState::Booting
                | MachineState::Restoring
                | MachineState::Running
                | MachineState::Stopping
        )
    }
}

/// What to start inside the guest after every boot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entrypoint {
    pub argv: Vec<String>,
    /// uid or user name. `None` is the volume's uid, else root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// A directory that survives stop/start on the worker holding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    /// Absolute guest path it is mounted at.
    pub path: String,
    /// Owner inside the guest.
    #[serde(default = "default_volume_uid")]
    pub uid: u32,
}

fn default_volume_uid() -> u32 {
    1000
}

/// Everything a worker needs to boot one machine. Sent in
/// `Down::AssignMachine`; persisted next to the volume so a restarted
/// worker can reattach.
///
/// `env` already has the caller's `secret_env` merged in, decrypted: this
/// is host-side data, like the credentials on a `SessionSpec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineSpec {
    pub machine_id: Uuid,
    /// VM name on the worker, `mch-<12 hex>`.
    pub name: String,
    #[serde(default)]
    pub engine: Engine,
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    /// Guest ports controld may open streams to.
    #[serde(default)]
    pub expose: Vec<u16>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Entrypoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<VolumeSpec>,
    /// Hard wall-clock cap; 0 is none.
    #[serde(default)]
    pub max_duration_s: u32,
    /// Keep the root disk across stop/start (Cloud Hypervisor only), so what
    /// was installed survives -- and is part of every snapshot.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persist_root: bool,
    /// Lay these disks down from a snapshot before booting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<crate::snapshot::RestoreOrder>,
    /// Bumped on every start. A worker reports it back on every state frame,
    /// and controld ignores a frame from an older boot -- so a late "stopped"
    /// from the previous run cannot mark a fresh one dead.
    pub generation: u64,
}

impl MachineSpec {
    /// The user commands run as when the caller names none.
    pub fn default_user(&self) -> Option<String> {
        self.volume.as_ref().map(|v| v.uid.to_string())
    }
}

/// VM name for a machine. Derived rather than looked up so a worker can name
/// a VM it has no spec for (the reaper's case). Distinct prefix from
/// sessions' `ses-` so drift detection can tell them apart.
pub fn machine_name_for(id: Uuid) -> String {
    format!("mch-{}", &id.simple().to_string()[..12])
}

/// Slots a machine of this size occupies. A slot is what workerd's hostcap
/// divides a host by -- one session VM, 2 vCPUs and 2 GiB -- so a machine
/// takes as many as whichever of its CPUs or memory is the larger multiple.
/// Counting memory alone let an 8-vCPU, 2 GiB machine pass for one session.
pub fn slots_for(cpus: u32, memory_mib: u32) -> u32 {
    memory_mib.div_ceil(2048).max(cpus.div_ceil(2)).max(1)
}

/// The filesystem-safe name an image is staged under:
/// `docker.io/poridhi/puku-agent:0.1.0` -> `docker.io_poridhi_puku-agent_0.1.0`.
///
/// Shared by workerd, which stages disks under it, and controld, which
/// checks a worker has the disk before placing a machine there. Must match
/// `key_for` in deploy/scripts/build-ch-rootfs.sh.
pub fn image_key(image: &str) -> String {
    image
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_distinct_from_sessions() {
        let id = Uuid::parse_str("43ffa9ff-b166-43bb-8506-e5f3d54c2846").unwrap();
        assert_eq!(machine_name_for(id), "mch-43ffa9ffb166");
    }

    #[test]
    fn slots_round_up_by_memory() {
        assert_eq!(slots_for(1, 512), 1);
        assert_eq!(slots_for(2, 2048), 1);
        assert_eq!(slots_for(2, 2049), 2);
        assert_eq!(slots_for(2, 4096), 2);
        assert_eq!(slots_for(2, 8192), 4);
    }

    /// A slot is 2 vCPUs as well as 2 GiB; the larger multiple wins.
    #[test]
    fn slots_round_up_by_cpus_too() {
        assert_eq!(slots_for(8, 2048), 4);
        assert_eq!(slots_for(8, 16384), 8);
        assert_eq!(slots_for(3, 1024), 2);
        assert_eq!(slots_for(0, 0), 1, "nothing is free");
    }

    #[test]
    fn image_keys_are_filesystem_safe_and_stable() {
        assert_eq!(image_key("docker.io/poridhi/puku-agent:0.1.0"), "docker.io_poridhi_puku-agent_0.1.0");
        assert_eq!(image_key("puku-agent:latest"), "puku-agent_latest");
        assert_eq!(image_key("img@sha256:abc"), "img_sha256_abc");
    }

    #[test]
    fn states_round_trip() {
        for s in [
            MachineState::Scheduled,
            MachineState::Booting,
            MachineState::Restoring,
            MachineState::Running,
            MachineState::Stopping,
            MachineState::Stopped,
            MachineState::Failed,
            MachineState::Destroyed,
        ] {
            assert_eq!(MachineState::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{}\"", s.as_str()));
        }
    }
}
