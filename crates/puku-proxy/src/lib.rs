//! Session proxy.
//!
//! Per docs/RELIABILITY-REBUILD.md §15.1. The proxy is stateful per session
//! (it holds the `last_seq` cursor and the `pending_question` state) but
//! stateless overall (no DB writes; reads only).
//!
//! ## Wire contract
//!
//! ```text
//! Client -> Proxy: WebSocket connect to /ws/sessions/<id>?token=...
//! Proxy  -> Client: hello{last_seq, instance_id, recovery_choice}
//! ... events stream ...
//! Client -> Proxy: hello{last_seq, reconnect_token}  (on reconnect)
//! Proxy  -> Client: replay events 1234..N from Postgres, then live
//! ```

pub mod events;
pub mod proxy;
pub mod reconnect;

pub use events::{Event, EventSink, EventSource, InMemoryEventSink};
pub use proxy::{ProxyConfig, RecoveryChoice, SessionProxy};
pub use reconnect::{InMemoryTokenStore, ReconnectToken, TokenStore};
