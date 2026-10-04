//! Keeping background loops alive.
//!
//! Sentry's panic hook fires on the panicking thread before unwinding, so it
//! does report a panic inside a `tokio::spawn`. What it cannot do is restart
//! the task -- and nothing here held a `JoinHandle`, so a panicked
//! dispatcher, scheduler or archive loop stopped for ever while the process
//! carried on reporting healthy. One Fatal event, then silence, and the
//! missing work is far worse than the missing report.
//!
//! This does not catch what does not unwind: OOM-kill, stack overflow,
//! `process::abort`. workerd boots microVMs, so an OOM-kill of the daemon is
//! a real possibility; that stays a `/metrics` and external-alert concern.

use std::time::Duration;

use sentry::{Hub, SentryFutureExt};

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Spawn a never-returning loop under supervision.
///
/// `make` is called afresh for each attempt, so the future it returns need
/// not be `Clone`. Each attempt runs on its own child hub, tagged with
/// `task`, so a crashed attempt's breadcrumbs do not bleed into the next.
pub fn supervise<F, Fut>(name: &'static str, mut make: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let outer = Hub::new_from_top(Hub::current());
    outer.configure_scope(|scope| {
        scope.set_tag("task", name);
        scope.set_transaction(Some(name));
    });

    tokio::spawn(
        async move {
            let mut backoff = INITIAL_BACKOFF;
            loop {
                let hub = Hub::new_from_top(Hub::current());
                match tokio::spawn(make().bind_hub(hub)).await {
                    // These loops are `loop { .. }` and are not supposed to
                    // return at all, so a clean return is itself a fault.
                    Ok(()) => {
                        tracing::error!(task = name, "supervised task returned; restarting")
                    }
                    Err(e) if e.is_panic() => {
                        // The panic event is already on its way, with the
                        // stacktrace and this task's tags. Log the restart
                        // so the churn shows up in the breadcrumb trail.
                        tracing::error!(task = name, "supervised task panicked; restarting")
                    }
                    // Cancelled: the runtime is shutting down.
                    Err(_) => return,
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        .bind_hub(outer),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// The behaviour that did not exist before: a panicking loop comes back.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_is_restarted() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        supervise("panicky", move || {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                panic!("boom");
            }
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            runs.load(Ordering::SeqCst) > 1,
            "a panicked task must be restarted, ran {} time(s)",
            runs.load(Ordering::SeqCst)
        );
    }

    /// A loop that returns cleanly is also a fault -- these never should.
    #[tokio::test(start_paused = true)]
    async fn a_task_that_returns_is_restarted_too() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        supervise("returner", move || {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(runs.load(Ordering::SeqCst) > 1);
    }

    /// Backoff must be capped, or a deterministically-failing task either
    /// spins hot or drifts to never retrying.
    #[test]
    fn backoff_is_capped() {
        let mut b = INITIAL_BACKOFF;
        for _ in 0..20 {
            b = (b * 2).min(MAX_BACKOFF);
        }
        assert_eq!(b, MAX_BACKOFF);
    }
}
