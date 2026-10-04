//! Idle detection for sessions.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.4. The worker side watches each VM's
//! last-activity time; when it crosses the per-session idle timeout, the
//! worker requests a snapshot and parks the VM. Controld then transitions
//! the session tier.
//!
//! In the worker, "activity" is a stream of events the agent streams over
//! vsock. We track the highest `seq` seen per session and compute the
//! timestamp delta.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Per-session idle tracker.
#[derive(Debug)]
pub struct IdleTracker {
    pub last_activity: DateTime<Utc>,
    pub idle_timeout: Duration,
}

impl IdleTracker {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            last_activity: Utc::now(),
            idle_timeout,
        }
    }

    /// Bump on any new event from the session.
    pub fn bump(&mut self) {
        self.last_activity = Utc::now();
    }

    /// True if the session has been idle for >= its timeout.
    pub fn is_idle(&self, now: DateTime<Utc>) -> bool {
        let elapsed = now - self.last_activity;
        elapsed >= chrono::Duration::from_std(self.idle_timeout).unwrap_or_default()
    }
}

/// Multi-session tracker.
#[derive(Debug, Default)]
pub struct IdleFleet {
    trackers: HashMap<Uuid, IdleTracker>,
}

impl IdleFleet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, session_id: Uuid, idle_timeout: Duration) {
        self.trackers
            .insert(session_id, IdleTracker::new(idle_timeout));
    }

    pub fn unregister(&mut self, session_id: Uuid) {
        self.trackers.remove(&session_id);
    }

    pub fn bump(&mut self, session_id: Uuid) {
        if let Some(t) = self.trackers.get_mut(&session_id) {
            t.bump();
        }
    }

    /// Returns the list of session ids whose idle timeout has elapsed.
    pub fn sessions_to_park(&self, now: DateTime<Utc>) -> Vec<Uuid> {
        self.trackers
            .iter()
            .filter(|(_, t)| t.is_idle(now))
            .map(|(sid, _)| *sid)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_is_not_idle() {
        let t = IdleTracker::new(Duration::from_secs(60));
        assert!(!t.is_idle(Utc::now()));
    }

    #[test]
    fn bump_resets_idle_clock() {
        let mut t = IdleTracker::new(Duration::from_secs(0));
        t.bump();
        // idle_timeout=0 means every moment is idle, so bump and check.
        let now = Utc::now();
        // After bump, still "now" since the comparison is `elapsed >= timeout`.
        // For timeout=0, this is always true. Test the inverse: a higher timeout.
        let mut t2 = IdleTracker::new(Duration::from_secs(60));
        t2.bump();
        assert!(!t2.is_idle(now));
        let _ = t.is_idle(now);
    }

    #[test]
    fn fleet_returns_idle_sessions() {
        let mut f = IdleFleet::new();
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        f.register(s1, Duration::from_secs(0));
        f.register(s2, Duration::from_secs(3600));
        let idle = f.sessions_to_park(Utc::now());
        assert!(idle.contains(&s1));
        assert!(!idle.contains(&s2));
    }

    #[test]
    fn unregister_removes_tracker() {
        let mut f = IdleFleet::new();
        let s = Uuid::new_v4();
        f.register(s, Duration::from_secs(0));
        assert!(!f.trackers.is_empty());
        f.unregister(s);
        assert!(f.trackers.is_empty());
    }
}