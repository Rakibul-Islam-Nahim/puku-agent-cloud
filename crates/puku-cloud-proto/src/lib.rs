//! Shared wire types for puku-agent-cloud.
//!
//! This crate is serde-only: no I/O, no async. It is the single contract
//! between controld, workerd, and the client CLI. Anything that crosses a
//! process boundary lives here.

pub mod client_ws;
pub mod data_proto;
pub mod engine;
pub mod event;
pub mod guest_proto;
pub mod machine;
pub mod session;
pub mod snapshot;
pub mod v2;
pub mod worker_proto;

pub use engine::Engine;
pub use event::{Event, EventKind};
pub use session::{SessionSpec, SessionState};
