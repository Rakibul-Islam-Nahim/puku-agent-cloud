//! Idle signal receiver. Per docs/RELIABILITY-REBUILD.md §4.4.3.
//!
//! The guest sends `idle_for_snapshot` over vsock when between tool calls.
//! The trigger may then capture early to reduce mid-tool pauses. This
//! module is a tiny in-process channel; the production wiring is over vsock.

use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct IdleSignal {
    pub session_id: uuid::Uuid,
    pub ts_unix: i64,
}

pub struct IdleChannel {
    tx: mpsc::UnboundedSender<IdleSignal>,
}

impl IdleChannel {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<IdleSignal>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    pub fn send(&self, sig: IdleSignal) {
        let _ = self.tx.send(sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_signal_round_trip() {
        let (ch, mut rx) = IdleChannel::new();
        ch.send(IdleSignal {
            session_id: uuid::Uuid::new_v4(),
            ts_unix: 42,
        });
        let s = rx.recv().await.unwrap();
        assert_eq!(s.ts_unix, 42);
    }
}