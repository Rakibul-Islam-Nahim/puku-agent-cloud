//! The session proxy itself.
//!
//! Stateless overall: holds no DB writes; reads events from an
//! `EventSource`, replays them on reconnect, hands a fresh token to the
//! client. Per-session state is just the in-memory `TokenStore`.

use std::sync::Arc;

use uuid::Uuid;

use crate::events::{Event, EventSink, EventSource};
use crate::reconnect::{ReconnectToken, TokenStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryChoice {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
pub struct HelloFrame {
    pub last_seq: u64,
    pub instance_id: String,
    pub recovery_choice: RecoveryChoice,
    pub token: ReconnectToken,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub instance_id: String,
}

impl ProxyConfig {
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
        }
    }
}

pub struct SessionProxy {
    pub cfg: ProxyConfig,
    pub source: Arc<dyn EventSource>,
    pub sink: Arc<dyn EventSink>,
    pub tokens: Arc<dyn TokenStore>,
}

impl SessionProxy {
    pub fn new(
        cfg: ProxyConfig,
        source: Arc<dyn EventSource>,
        sink: Arc<dyn EventSink>,
        tokens: Arc<dyn TokenStore>,
    ) -> Self {
        Self {
            cfg,
            source,
            sink,
            tokens,
        }
    }

    /// Open a fresh session: issue a token at `last_seq=0`, send a hello,
    /// start streaming events.
    pub async fn open(
        &self,
        session_id: Uuid,
        recovery_choice: RecoveryChoice,
    ) -> Result<HelloFrame, String> {
        let token = self.tokens.issue(session_id, 0).await;
        Ok(HelloFrame {
            last_seq: 0,
            instance_id: self.cfg.instance_id.clone(),
            recovery_choice,
            token,
        })
    }

    /// Reconnect: validate the token, replay events > last_seq.
    pub async fn replay(
        &self,
        token: ReconnectToken,
    ) -> Result<(HelloFrame, Vec<Event>), String> {
        let state = self
            .tokens
            .lookup(&token)
            .await
            .ok_or_else(|| "unknown reconnect token".to_string())?;
        let events = self.source.events_after(state.session_id, state.last_seq).await?;
        Ok((
            HelloFrame {
                last_seq: state.last_seq,
                instance_id: self.cfg.instance_id.clone(),
                recovery_choice: RecoveryChoice::Local, // default
                token: token.clone(),
            },
            events,
        ))
    }

    /// Deliver an event to the client and advance the cursor.
    pub async fn deliver(&self, token: &ReconnectToken, ev: &Event) -> Result<(), String> {
        self.sink.write(ev).await?;
        self.tokens.advance(token, ev.seq).await
    }
}

// Helper for tests: replay with an overridden source.
impl SessionProxy {
    pub async fn replay_with_source(
        &self,
        token: ReconnectToken,
        source: Arc<dyn EventSource>,
    ) -> Result<(HelloFrame, Vec<Event>), String> {
        let state = self
            .tokens
            .lookup(&token)
            .await
            .ok_or_else(|| "unknown reconnect token".to_string())?;
        let events = source.events_after(state.session_id, state.last_seq).await?;
        Ok((
            HelloFrame {
                last_seq: state.last_seq,
                instance_id: self.cfg.instance_id.clone(),
                recovery_choice: RecoveryChoice::Local,
                token,
            },
            events,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::InMemoryEventSink;
    use crate::reconnect::InMemoryTokenStore;
    use std::collections::HashMap;

    struct StubSource {
        by_session: HashMap<Uuid, Vec<Event>>,
    }
    #[async_trait::async_trait]
    impl EventSource for StubSource {
        async fn events_after(
            &self,
            session_id: Uuid,
            after_seq: u64,
        ) -> Result<Vec<Event>, String> {
            Ok(self
                .by_session
                .get(&session_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e.seq > after_seq)
                .collect())
        }
    }

    #[tokio::test]
    async fn open_then_deliver_then_replay_replays_only_new_events() {
        let sid = Uuid::new_v4();
        let ev1 = Event {
            session_id: sid,
            seq: 1,
            kind: "agent".into(),
            payload: serde_json::json!({"hello": "world"}),
        };
        let ev2 = Event {
            session_id: sid,
            seq: 2,
            kind: "session".into(),
            payload: serde_json::json!({}),
        };
        let source = Arc::new(StubSource {
            by_session: [(sid, vec![ev1.clone(), ev2.clone()])]
                .into_iter()
                .collect(),
        });
        let sink = Arc::new(InMemoryEventSink::new());
        let tokens = Arc::new(InMemoryTokenStore::new());
        let proxy = SessionProxy::new(
            ProxyConfig::new("instance-1"),
            source,
            sink.clone(),
            tokens.clone(),
        );

        let hello = proxy.open(sid, RecoveryChoice::Local).await.unwrap();
        assert_eq!(hello.last_seq, 0);

        // Deliver both events through the proxy.
        proxy.deliver(&hello.token, &ev1).await.unwrap();
        proxy.deliver(&hello.token, &ev2).await.unwrap();

        // Simulate client reconnect: token already at seq=2.
        let (replay_hello, replayed) = proxy.replay(hello.token.clone()).await.unwrap();
        assert_eq!(replay_hello.last_seq, 2);
        assert!(replayed.is_empty(), "no new events to replay");

        // Insert a new event and replay again.
        let ev3 = Event {
            session_id: sid,
            seq: 3,
            kind: "agent".into(),
            payload: serde_json::json!({"again": true}),
        };
        let (_, replayed) = proxy
            .replay_with_source(
                hello.token.clone(),
                std::sync::Arc::new(StubSource {
                    by_session: [(sid, vec![ev1.clone(), ev2.clone(), ev3.clone()])]
                        .into_iter()
                        .collect(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].seq, 3);
    }
}