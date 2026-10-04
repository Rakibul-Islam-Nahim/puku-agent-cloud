//! The worker data plane: byte streams between controld and a machine.
//!
//! The control link (`/v1/worker`) is one JSON socket carrying every
//! session's events and heartbeats, with an unbounded queue and no
//! backpressure. A noVNC stream or a workspace tarball on it would stall all
//! of that. So bulk traffic rides separate sockets instead, and workers still
//! only ever dial out:
//!
//! 1. A worker keeps a few idle sockets open to `/v1/worker/data`, each
//!    opened with a [`DataHello`] text frame.
//! 2. To reach a machine, controld takes an idle socket for that worker and
//!    sends a [`StreamHeader`] text frame naming what it wants.
//! 3. Both sides then exchange binary frames (raw bytes) and [`DataMsg`]
//!    text frames (framing and results) until one closes the socket. A
//!    socket carries exactly one stream; the worker dials a replacement as
//!    soon as it sees a header.
//!
//! When the pool for a worker is empty, controld asks for more with
//! `Down::OpenDataSockets` on the control link and waits for one to arrive.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// First frame a worker sends on a new data socket. Authenticated exactly
/// like `Up::Register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataHello {
    pub worker_name: String,
    pub auth_token: String,
}

/// First frame controld sends when it puts an idle socket to use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamHeader {
    pub stream_id: Uuid,
    pub machine_id: Uuid,
    pub target: StreamTarget,
}

/// What a stream is for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StreamTarget {
    /// Raw TCP to `127.0.0.1:port` in the guest, both directions, until
    /// either side closes.
    Port { port: u16 },
    /// Run a command. When `stdin` is true controld sends the input as binary
    /// frames followed by `DataMsg::Eof`. The worker answers with one
    /// `DataMsg::ExecResult`.
    Exec {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        timeout_ms: u64,
        #[serde(default)]
        stdin: bool,
    },
    /// Stream a file out: binary frames, then `Eof` -- or one `Error`
    /// before any bytes (404, 400 for a directory, 413 over `max_bytes`).
    FileRead {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },
    /// Stream a file in: controld sends binary frames then `Eof`; the worker
    /// answers `Done` or `Error`.
    FileWrite { path: String, mode: u32 },
    /// One `Listing` or `Error`.
    List { path: String, recursive: bool },
    /// A tar.gz of a directory's contents out, framed like `FileRead`.
    ArchiveGet {
        path: String,
        #[serde(default)]
        excludes: Vec<String>,
    },
    /// A tar.gz in, framed like `FileWrite`.
    ArchivePut { path: String, replace: bool },
}

/// Text frames exchanged after the header.
///
/// The worker's first frame after a header is always `Ready` or `Error`:
/// it checks the machine, the port, the path before anything flows, so a
/// missing file is a clean 404 rather than a stream that ends early -- and a
/// pooled socket whose worker side died is noticed here, before the caller
/// has committed to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataMsg {
    /// The target is valid and the worker is ready for (or about to send)
    /// the stream's bytes.
    Ready,
    /// The sender has no more bytes for this stream.
    Eof,
    /// The operation failed. `status` is the HTTP status it maps to.
    Error { status: u16, message: String },
    ExecResult {
        code: i32,
        stdout: String,
        stderr: String,
        timed_out: bool,
        truncated: bool,
    },
    Listing { entries: Vec<FileEntry> },
    /// A write or extract finished.
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    /// `file` or `dir`.
    pub kind: String,
    pub size: u64,
    pub executable: bool,
}

/// Cap on each of an exec's stdout and stderr, applied worker-side.
pub const MAX_EXEC_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Largest binary frame either side sends. Small enough to interleave with
/// pings, large enough that a tarball is not a million frames.
pub const DATA_CHUNK_BYTES: usize = 256 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_tagged_by_op() {
        let h = StreamHeader {
            stream_id: Uuid::nil(),
            machine_id: Uuid::nil(),
            target: StreamTarget::Port { port: 6080 },
        };
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains(r#""op":"port""#), "{json}");
        assert_eq!(serde_json::from_str::<StreamHeader>(&json).unwrap(), h);
    }

    #[test]
    fn messages_are_tagged_by_type() {
        let json = serde_json::to_string(&DataMsg::Error { status: 404, message: "x".into() }).unwrap();
        assert_eq!(json, r#"{"type":"error","status":404,"message":"x"}"#);
        assert_eq!(serde_json::from_str::<DataMsg>(r#"{"type":"eof"}"#).unwrap(), DataMsg::Eof);
    }
}
