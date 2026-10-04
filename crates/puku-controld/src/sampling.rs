//! Which transactions are worth recording.
//!
//! There is no tail-sampling in sentry-rust: `traces_sampler` runs when a
//! transaction *starts*, long before anyone knows whether it failed, and
//! there is no `before_send_transaction` to reconsider afterwards. The
//! "always sample failures, rarely sample the rest" idiom is Python/JS only.
//!
//! It matters less than it sounds. Error events are sampled separately
//! (`sample_rate` stays at 1.0) and an error carries the trace id from its
//! scope even when the surrounding transaction was dropped -- so a failure
//! is always visible and always says which trace it belonged to. What is
//! lost is only the waterfall for that particular one.
//!
//! What the rates are actually defending against is volume: the 3s dispatch
//! loop alone would be 28,800 transactions a day, and the container
//! HEALTHCHECK curls `/health` every 30 seconds.

use sentry::TransactionContext;

pub fn traces_sampler(ctx: &TransactionContext) -> f32 {
    // Honour an inherited decision, or a distributed trace gets sampled in
    // half at the service boundary and the waterfall has holes in it.
    if let Some(sampled) = ctx.sampled() {
        return if sampled { 1.0 } else { 0.0 };
    }

    let name = ctx.name();
    match ctx.operation() {
        "http.server" => match name {
            // Pure machine noise: the HEALTHCHECK and Prometheus.
            "GET /health" | "GET /metrics" => 0.0,
            // Long-lived sockets. The transaction ends at the 101, so it
            // records nothing about the session that follows.
            "GET /v1/worker" | "GET /v1/sessions/{id}/attach" => 0.0,
            // The writes worth measuring. Session creation is the one
            // operation whose latency anyone ever asks about.
            n if n.starts_with("POST /v1/sessions") => 0.20,
            n if n.starts_with("POST /v1/schedules") || n.starts_with("POST /v1/hooks/") => 0.20,
            n if n.contains(" /v1/") => 0.05,
            // Unmatched paths: scanner traffic that MatchedPath cannot
            // collapse, so /wp-login.php and /.env each become their own
            // transaction name. Unbounded cardinality; never sample.
            _ => 0.0,
        },
        _ => match name {
            "dispatch_pending" | "scheduler" | "archive" | "housekeeping" | "notify_listener" => {
                0.005
            }
            _ => 0.02,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rates only matter if the loud, worthless things are actually
    /// silenced -- these four are the entire volume problem.
    #[test]
    fn machine_noise_is_never_sampled() {
        for (op, name) in [
            ("http.server", "GET /health"),
            ("http.server", "GET /metrics"),
            ("http.server", "GET /v1/worker"),
            ("http.server", "GET /v1/sessions/{id}/attach"),
        ] {
            let ctx = TransactionContext::new(name, op);
            assert_eq!(traces_sampler(&ctx), 0.0, "{name} should not be sampled");
        }
    }

    /// A path MatchedPath could not collapse is unbounded cardinality.
    #[test]
    fn unmatched_scanner_paths_are_dropped() {
        let ctx = TransactionContext::new("GET /wp-login.php", "http.server");
        assert_eq!(traces_sampler(&ctx), 0.0);
    }

    #[test]
    fn session_writes_are_sampled_more_than_reads() {
        let write = TransactionContext::new("POST /v1/sessions", "http.server");
        let read = TransactionContext::new("GET /v1/sessions/{id}/events", "http.server");
        assert!(traces_sampler(&write) > traces_sampler(&read));
        assert!(traces_sampler(&read) > 0.0, "reads should not be silenced entirely");
    }

    /// 28,800/day from one loop is what this rate exists to prevent.
    #[test]
    fn background_loops_are_nearly_silent() {
        let ctx = TransactionContext::new("dispatch_pending", "default");
        assert!(traces_sampler(&ctx) <= 0.01);
    }
}
