//! Client attach WebSocket protocol (`WS /v1/sessions/{id}/attach`).
//!
//! Flow: client sends `Hello{after_seq}`; server replays persisted events
//! with `seq > after_seq`, sends `Live`, then tails the in-process fanout.
//! Events arriving during replay are buffered and deduped by `seq`, so the
//! stream is gap-free and strictly ordered.

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::session::SessionState;

/// Client -> server messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Must be the first message. `after_seq: 0` replays everything;
    /// `after_seq: -1` skips replay (live only).
    Hello { after_seq: i64 },
    /// Answer to a pending question (permission prompt, AskUserQuestion,
    /// plan gate). `question_id` is the `request_id` from the session's
    /// `pending_question`; the server turns this into the `control_response`
    /// frame puku-cli is blocked on. Set `decision: "deny"` to refuse the
    /// tool instead of answering it.
    Answer {
        question_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// Per-question answers keyed by each question's `header`; use when
        /// the ask carries more than one question.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<std::collections::HashMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Free-form follow-up user message.
    Input { text: String },
    Interrupt,
}

/// Server -> client messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// A batch of events, ordered by seq (used for both replay and live).
    Events { events: Vec<Event> },
    /// Replay finished; everything after this is live.
    Live,
    /// Session state changed (also emitted as a `session` event; this is a
    /// convenience for clients that only render state).
    State {
        state: SessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Error { message: String },
}
