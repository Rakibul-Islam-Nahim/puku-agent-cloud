//! Volume errors.

use crate::types::HostId;

#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error("volume {0} not found")]
    NotFound(String),
    #[error("host {0} already fenced")]
    HostFenced(HostId),
    #[error("i/o error: {0}")]
    Io(String),
    #[error("fence failed: {0}")]
    Fence(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("snapshot failed: {0}")]
    Snapshot(String),
    #[error("backend rejected operation: {0}")]
    Rejected(String),
}