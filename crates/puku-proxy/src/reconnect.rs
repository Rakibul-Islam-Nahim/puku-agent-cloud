//! Reconnect tokens. Per docs/RELIABILITY-REBUILD.md §15.1.
//!
//! A `ReconnectToken` is opaque to the client; the proxy uses it on
//! reconnect to look up the last known `last_seq` and replay events.

use async_trait::async_trait;
use uuid::Uuid;

/// A reconnect token (just a UUID in our impl; the contract is opaque).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReconnectToken(pub Uuid);

impl ReconnectToken {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ReconnectToken {
    fn default() -> Self {
        Self::new()
    }
}

/// State the proxy keeps per (session, token) pair.
#[derive(Debug, Clone)]
pub struct TokenState {
    pub session_id: Uuid,
    pub last_seq: u64,
    pub token: ReconnectToken,
}

#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Issue a fresh token for `session_id` at the current `last_seq`.
    async fn issue(&self, session_id: Uuid, last_seq: u64) -> ReconnectToken;
    /// Look up the last known `last_seq` for this token.
    async fn lookup(&self, token: &ReconnectToken) -> Option<TokenState>;
    /// Update the cursor after a successful delivery.
    async fn advance(&self, token: &ReconnectToken, last_seq: u64) -> Result<(), String>;
}

/// In-memory token store. Production: a Postgres table keyed by token.
pub struct InMemoryTokenStore {
    pub by_token: tokio::sync::Mutex<std::collections::HashMap<ReconnectToken, TokenState>>,
}

impl InMemoryTokenStore {
    pub fn new() -> Self {
        Self {
            by_token: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenStore for InMemoryTokenStore {
    async fn issue(&self, session_id: Uuid, last_seq: u64) -> ReconnectToken {
        let t = ReconnectToken::new();
        let mut g = self.by_token.lock().await;
        g.insert(
            t.clone(),
            TokenState {
                session_id,
                last_seq,
                token: t.clone(),
            },
        );
        t
    }
    async fn lookup(&self, token: &ReconnectToken) -> Option<TokenState> {
        self.by_token.lock().await.get(token).cloned()
    }
    async fn advance(&self, token: &ReconnectToken, last_seq: u64) -> Result<(), String> {
        let mut g = self.by_token.lock().await;
        match g.get_mut(token) {
            Some(s) => {
                s.last_seq = last_seq;
                Ok(())
            }
            None => Err("token not found".into()),
        }
    }
}