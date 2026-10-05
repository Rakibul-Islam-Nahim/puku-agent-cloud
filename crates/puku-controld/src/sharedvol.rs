//! Sessions whose disk is an RBD image on the shared Ceph cluster.
//!
//! A session's disk used to exist on one host's disk and nowhere else, so
//! a resume could only ever go back there. Workers that advertise
//! `FEATURE_SHARED_VOLUMES` keep it in Ceph instead, which lets a session
//! continue on another host -- but only safely:
//!
//! - **Home connected:** the session goes back to the host that last ran it
//!   (its disk was released when it stopped; going home keeps caches warm
//!   and never needs a fence).
//! - **Home declared dead** (its lease is `released`), or gone past the
//!   volume grace with no lease to say otherwise: the old host is cut off
//!   from this disk at the Ceph layer first (`Fence::fence_volumes`), and
//!   only then is the session placed on any shared-volume worker. A fence
//!   that fails leaves the session queued: never two writers.
//! - **Home only briefly away** (lease still held or suspected): wait.
//!   Fencing a host that is alive would cut every disk it has open, not
//!   just this one.

use std::sync::Arc;

use puku_fence::Fence;
use puku_leases::{LeaseState, LeaseStore};
use puku_volume::VolumeId;
use uuid::Uuid;

use crate::AppState;

/// How long a host with no lease may be gone before its sessions move.
/// Same as the host-local volume grace: without a lease nothing says the
/// host is dead rather than rebooting.
const NO_LEASE_GRACE: chrono::Duration = chrono::Duration::minutes(15);

pub struct SharedVolumes {
    /// The pool every shared-volume worker keeps session images in.
    pub pool: String,
    pub fence: Arc<dyn Fence>,
}

impl SharedVolumes {
    pub fn volume_of(&self, session_id: Uuid) -> VolumeId {
        VolumeId(format!("{}/{}", self.pool, session_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveDecision {
    Move,
    Wait(String),
}

/// Whether a shared-disk session may leave `home`, which is not connected.
pub async fn may_move(state: &AppState, home: Uuid) -> anyhow::Result<MoveDecision> {
    let store = crate::leases::PgLeaseStore { pool: state.pool.clone() };
    match store.load(home).await.map_err(|e| anyhow::anyhow!("{e}"))? {
        Some(l) if l.state == LeaseState::Released => return Ok(MoveDecision::Move),
        Some(_) => {
            return Ok(MoveDecision::Wait(
                "its last host is away but not yet declared dead; waiting rather than fencing a live host".into(),
            ))
        }
        None => {}
    }
    let gone_for = match crate::db::worker_presence(&state.pool, home).await? {
        Some((_, _, Some(hb))) => chrono::Utc::now() - hb,
        _ => NO_LEASE_GRACE + chrono::Duration::seconds(1),
    };
    if gone_for > NO_LEASE_GRACE {
        Ok(MoveDecision::Move)
    } else {
        Ok(MoveDecision::Wait("its last host has been away only briefly".into()))
    }
}

/// Cut `home` off from the session's disk. Must succeed before the session
/// is placed anywhere else.
pub async fn fence_for_move(state: &AppState, home: Uuid, session_id: Uuid) -> anyhow::Result<()> {
    let Some(shared) = &state.shared_volumes else {
        anyhow::bail!("this control plane has no Ceph access configured (PUKU_RBD_POOL), so it cannot fence");
    };
    let vol = shared.volume_of(session_id);
    let receipt = shared
        .fence
        .fence_volumes(home, Some(session_id), &[vol], None)
        .await
        .map_err(|e| anyhow::anyhow!("fencing the old host off the session's disk failed: {e}"))?;
    tracing::warn!(
        session = %session_id, old_host = %home, clients = ?receipt.clients,
        "fenced the old host off the session's disk; moving the session"
    );
    Ok(())
}
