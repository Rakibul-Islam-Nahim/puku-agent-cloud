//! In-process live event fanout: one broadcast channel per active session.
//! Persistence happens before publish (in db::*), so a subscriber that also
//! replays from Postgres can always produce a gap-free, deduped stream.

use puku_cloud_proto::event::Event;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use uuid::Uuid;

const CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct SessionHub {
    inner: Arc<Mutex<HashMap<Uuid, broadcast::Sender<Event>>>>,
}

impl SessionHub {
    pub fn new() -> Self {
        SessionHub { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn publish(&self, ev: &Event) {
        let mut map = self.inner.lock().unwrap();
        if let Some(tx) = map.get(&ev.session_id) {
            // Send fails only when there are no subscribers; drop the
            // channel then so idle sessions don't accumulate.
            if tx.send(ev.clone()).is_err() && tx.receiver_count() == 0 {
                map.remove(&ev.session_id);
            }
        }
    }

    pub fn publish_all(&self, events: &[Event]) {
        for ev in events {
            self.publish(ev);
        }
    }

    pub fn subscribe(&self, session_id: Uuid) -> broadcast::Receiver<Event> {
        let mut map = self.inner.lock().unwrap();
        map.entry(session_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    fn has_subscribers(&self, session_id: Uuid) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|tx| tx.receiver_count() > 0)
            .unwrap_or(false)
    }
}

impl crate::AppState {
    /// Publish persisted events to local subscribers and NOTIFY other
    /// controld instances so their attached clients get them too.
    pub async fn publish_events(&self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        self.hub.publish_all(events);
        let first = &events[0];
        let last = events.last().unwrap();
        let payload = serde_json::json!({
            "i": self.cfg.instance_id,
            "s": first.session_id,
            "f": first.seq,
            "t": last.seq,
        });
        if let Err(e) = sqlx::query("SELECT pg_notify('puku_events', $1)")
            .bind(payload.to_string())
            .execute(&self.pool)
            .await
        {
            tracing::debug!(error = %e, "event notify failed");
        }
    }
}

/// Cross-instance fanout: listen for event ranges published by other
/// controld instances and replay them into the local hub (subscribers
/// dedup by seq, so overlap with local publishes is harmless).
pub fn spawn_notify_listener(state: crate::AppState, db_url: String) {
    tokio::spawn(async move {
        loop {
            let mut listener = match sqlx::postgres::PgListener::connect(&db_url).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "notify listener connect failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            if let Err(e) = listener.listen("puku_events").await {
                tracing::warn!(error = %e, "listen failed");
                continue;
            }
            while let Ok(n) = listener.recv().await {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(n.payload()) else { continue };
                let same_instance = v["i"].as_str()
                    == Some(state.cfg.instance_id.to_string().as_str());
                if same_instance {
                    continue;
                }
                let (Some(sid), Some(from), Some(to)) =
                    (v["s"].as_str(), v["f"].as_i64(), v["t"].as_i64())
                else {
                    continue;
                };
                let Ok(session_id) = Uuid::parse_str(sid) else { continue };
                if !state.hub.has_subscribers(session_id) {
                    continue;
                }
                match crate::db::fetch_events(&state.pool, session_id, from - 1, to - from + 1).await
                {
                    Ok(events) => state.hub.publish_all(&events),
                    Err(e) => tracing::debug!(error = %e, "cross-instance fetch failed"),
                }
            }
            tracing::warn!("notify listener dropped; reconnecting");
        }
    });
}
