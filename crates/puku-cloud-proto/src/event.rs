use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Origin of an event in a session's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A verbatim puku-cli stream-json line. `payload` is the parsed line,
    /// never rewritten by the platform.
    Agent,
    /// Platform lifecycle event, e.g. `{"type":"session.state","state":"running"}`.
    Session,
    /// In-guest process event, e.g. `{"type":"exec.exited","code":0}`.
    Exec,
    /// User input echoed into the log so replay shows the full conversation.
    User,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Agent => "agent",
            EventKind::Session => "session",
            EventKind::Exec => "exec",
            EventKind::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "agent" => EventKind::Agent,
            "session" => EventKind::Session,
            "exec" => EventKind::Exec,
            "user" => EventKind::User,
            _ => return None,
        })
    }
}

/// One entry in a session's append-only event log.
///
/// `seq` is the global per-session ordering, allocated by controld at insert
/// time (`sessions.last_seq` incremented transactionally) — clients replay
/// with it. Events that originate in the guest also carry `guest_line`, the
/// 1-based line number in `/session/events.ndjson`; `(session_id,
/// guest_line)` is unique, which makes worker redelivery idempotent and
/// gives reconnecting workers their resume cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub session_id: Uuid,
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    /// Line number in the in-guest outbox file, for guest-originated events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_line: Option<i64>,
    /// Set when the payload was truncated and the full content lives in
    /// object storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_ref: Option<String>,
}

/// A guest-originated event as shipped worker→controld, before controld
/// assigns the global `seq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestEvent {
    /// 1-based line number in `/session/events.ndjson`.
    pub line: i64,
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_ref: Option<String>,
}
