//! Host <-> guest-agent protocol, for engines whose guests run `puku-guestd`
//! (Cloud Hypervisor). msb brings its own in-guest agent and does not use
//! this.
//!
//! One stream per request. For Cloud Hypervisor the stream is a vsock
//! connection: the host opens the VM's hybrid-vsock unix socket, writes
//! `CONNECT <GUEST_AGENT_PORT>\n`, reads back `OK <n>\n`, and from then on
//! the socket is a byte pipe to the agent.
//!
//! On that pipe both sides exchange frames: a 5-byte header (kind, then a
//! big-endian u32 length) and the payload. A request is one `JSON` frame
//! holding a [`GuestRequest`]; what follows depends on it:
//!
//! * `exec`: the host may send `STDIN` frames and one `STDIN_EOF`; the agent
//!   sends `STDOUT`/`STDERR` frames and finally one `JSON` [`GuestReply::Exited`].
//! * `connect`: the agent answers one `JSON` `Ok` (or `Error`), then the
//!   stream stops being framed and is spliced to the guest port raw.
//! * everything else: one `JSON` reply.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The vsock port puku-guestd listens on.
pub const GUEST_AGENT_PORT: u32 = 1024;

/// Largest frame payload either side accepts.
pub const MAX_FRAME: usize = 1 << 20;

pub mod kind {
    pub const JSON: u8 = 1;
    pub const STDIN: u8 = 2;
    pub const STDOUT: u8 = 3;
    pub const STDERR: u8 = 4;
    pub const STDIN_EOF: u8 = 5;
}

pub fn header(kind: u8, len: usize) -> [u8; 5] {
    let n = (len as u32).to_be_bytes();
    [kind, n[0], n[1], n[2], n[3]]
}

pub fn parse_header(h: [u8; 5]) -> Result<(u8, usize), String> {
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if len > MAX_FRAME {
        return Err(format!("frame of {len} bytes exceeds the {MAX_FRAME} limit"));
    }
    if !(kind::JSON..=kind::STDIN_EOF).contains(&h[0]) {
        return Err(format!("unknown frame kind {}", h[0]));
    }
    Ok((h[0], len))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GuestRequest {
    Ping,
    /// The environment and hostname every later command starts from. Sent
    /// by the host right after boot, over vsock rather than on the kernel
    /// command line, so credentials never appear in `/proc/cmdline`.
    Init {
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        hostname: String,
    },
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// uid, `uid:gid`, or a name from the guest's /etc/passwd.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        /// Whether the host will send `STDIN` frames. Without it the
        /// command's stdin is /dev/null.
        #[serde(default)]
        stdin: bool,
    },
    /// Splice this stream to `127.0.0.1:port` inside the guest.
    Connect { port: u16 },
    /// Sync and power off.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestReply {
    Ok,
    Pong { version: String },
    /// A command finished. A timeout kills its whole process group and
    /// reports code 124, as coreutils `timeout` does.
    Exited { code: i32, timed_out: bool },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip() {
        let h = header(kind::STDOUT, 70_000);
        assert_eq!(parse_header(h), Ok((kind::STDOUT, 70_000)));
    }

    #[test]
    fn oversized_and_unknown_frames_are_refused() {
        assert!(parse_header(header(kind::JSON, MAX_FRAME + 1)).is_err());
        assert!(parse_header([9, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn requests_are_tagged_by_op() {
        let r = GuestRequest::Connect { port: 6080 };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"op":"connect","port":6080}"#);
        let e: GuestRequest =
            serde_json::from_str(r#"{"op":"exec","argv":["id"]}"#).unwrap();
        assert!(matches!(e, GuestRequest::Exec { stdin: false, .. }));
    }
}
