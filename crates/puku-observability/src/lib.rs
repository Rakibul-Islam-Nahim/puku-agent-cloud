//! Tracing and error reporting, shared by every puku cloud binary.
//!
//! This exists because a week of testing found bugs that were all invisible
//! in the same way. Sessions finished and never reported it. A credential
//! went out in a header the gateway ignores and surfaced as a 401 inside a
//! guest. A clone failed with `exit 70` and the reason lived only in a file
//! on the box. None produced an alert, a metric, or a log line anyone found.
//!
//! The structural cause was that errors had nowhere to go: `AppError`
//! serialised 500s to the client and logged nothing, nothing supervised a
//! `tokio::spawn`, and `session_id` appeared under three different field
//! names so it could not even be grepped reliably.
//!
//! Everything here is inert without `SENTRY_DSN`. That is deliberate: the
//! client is created disabled, every `sentry::` call becomes a no-op, and a
//! deployment that never sets a DSN behaves exactly as it did before.

mod scrub;
mod supervise;

pub use scrub::ALLOWED_FIELDS;
pub use supervise::supervise;

use std::borrow::Cow;

use sentry::ClientInitGuard;

/// Start Sentry, reading the DSN from `SENTRY_DSN`.
///
/// The returned guard **must** be held for the life of the process. Dropping
/// it disposes the client, and every later event is silently discarded --
/// `let _ = init_sentry(..)` drops immediately and `#[must_use]` does not
/// warn on `let _`. Bind it to a named `_guard`.
///
/// Call this *before* building the tokio runtime: the panic hook is then
/// installed before any task can exist, failures during startup are
/// captured, and the guard's blocking flush on drop happens on the main
/// thread rather than on a runtime worker. Sentry's own transport spawns a
/// dedicated thread with its own current-thread runtime, so it never touches
/// ours.
/// Start Sentry and the tracing subscriber, in that order, and say which
/// way it went.
///
/// One call because the order is not optional and getting it wrong is
/// silent: `init_sentry` used to log "sentry enabled" itself, before any
/// subscriber existed, so the line went nowhere and an operator had no way
/// to tell a working DSN from a typo'd one.
#[must_use = "dropping the guard disposes the client and discards every event"]
pub fn init(service: &'static str, sampler: fn(&sentry::TransactionContext) -> f32) -> ClientInitGuard {
    let guard = init_sentry(service, sampler);
    init_tracing();
    if guard.is_enabled() {
        tracing::info!(service, "sentry enabled");
    } else {
        // Worth saying out loud. An unset or malformed DSN is indis-
        // tinguishable from a working one at every other point.
        tracing::info!(service, "sentry disabled (SENTRY_DSN unset or unparseable)");
    }
    guard
}

#[must_use = "dropping the guard disposes the client and discards every event"]
fn init_sentry(service: &'static str, sampler: fn(&sentry::TransactionContext) -> f32) -> ClientInitGuard {
    let mut opts = sentry::ClientOptions::new()
        // Deliberately NOT calling .dsn(): it panics on a malformed value.
        // Letting apply_defaults read SENTRY_DSN turns a typo into "Sentry
        // is off" rather than a crash-looping unit.
        .maybe_release(sentry::release_name!())
        .environment(
            std::env::var("SENTRY_ENVIRONMENT").unwrap_or_else(|_| "production".into()),
        )
        // ---- privacy floor ----
        .send_default_pii(false)
        .max_request_body_size(sentry::MaxRequestBodySize::None)
        .max_breadcrumbs(50)
        // ---- error shape ----
        .attach_stacktrace(true)
        .in_app_include(vec!["puku_controld", "puku_workerd", "puku_cloud_proto"])
        .in_app_exclude(vec!["tokio", "hyper", "axum", "sqlx", "tower", "reqwest"])
        // Never sample away an error. Traces are sampled separately, and an
        // error still carries the trace id even when its transaction was not
        // sampled -- so a failure is always visible.
        .sample_rate(1.0)
        .traces_sampler(sampler)
        .before_send(scrub::before_send)
        .before_breadcrumb(scrub::before_breadcrumb)
        // Transactions never pass through before_send -- sentry-rust has no
        // before_send_transaction hook at all -- so the transport is the
        // only place left to scrub them.
        .transport(scrub::ScrubbingTransportFactory);

    if let Some(name) = server_name() {
        opts = opts.server_name(name);
    }

    let _ = service;
    sentry::init(opts)
}

fn server_name() -> Option<Cow<'static, str>> {
    std::env::var("PUKU_WORKER_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(Cow::Owned)
}

/// Install the tracing subscriber: console output plus, when Sentry is
/// enabled, breadcrumbs, events and spans.
fn init_tracing() {
    use sentry::integrations::tracing::EventFilter;
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter, Layer, Registry};

    // Per-layer filters rather than one on the Registry. A global EnvFilter
    // gates every layer, so `RUST_LOG=warn` would starve Sentry of the INFO
    // breadcrumbs and spans it needs -- turning console verbosity down would
    // quietly turn error reporting down with it.
    let console = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let sentry_layer = sentry::integrations::tracing::layer()
        // error! raises an issue; warn!/info! attach as breadcrumbs to
        // whatever error comes next. All 11 existing error! sites are
        // genuine failures, while the ~55 warn! sites are retries and
        // fallbacks that fire routinely -- alerting on those means the
        // inbox stops being read, and then the real events are missed too.
        .event_filter(|md| match *md.level() {
            tracing::Level::ERROR => EventFilter::Event,
            tracing::Level::WARN | tracing::Level::INFO => EventFilter::Breadcrumb,
            _ => EventFilter::Ignore,
        })
        // Our spans only. sqlx's query spans carry SQL and bound parameters,
        // and span fields land in transaction data with no hook to strip
        // them afterwards.
        .span_filter(|md| {
            matches!(
                *md.level(),
                tracing::Level::INFO | tracing::Level::WARN | tracing::Level::ERROR
            ) && md.target().starts_with("puku_")
        });
    // Deliberately not .enable_span_attributes(): it copies every parent
    // span's fields onto child events, widening the leak surface for no gain.

    Registry::default()
        .with(fmt::layer().with_filter(console))
        .with(sentry_layer.with_filter(LevelFilter::INFO))
        .init();
}

/// Tag the current scope with the caller's identity.
///
/// Correlation here is by tag, not by distributed trace: the worker protocol
/// carries no trace id, and a session lives for minutes to hours -- far too
/// long to model as one transaction. Tagging every event in all three
/// services with the same `session_id` makes one session's failures
/// searchable across them, which is the thing anyone actually wants.
pub fn tag_session(session_id: impl std::fmt::Display) {
    sentry::configure_scope(|scope| scope.set_tag("session_id", session_id.to_string()));
}

/// Tag the current scope with the org a request belongs to.
pub fn tag_org(org_id: impl std::fmt::Display) {
    sentry::configure_scope(|scope| scope.set_tag("org_id", org_id.to_string()));
}

#[cfg(test)]
mod leak_tests {
    /// The end-to-end version of the whitelist argument.
    ///
    /// The unit tests in `scrub` cover the map in isolation; this drives a
    /// real `tracing::error!` through a real client and inspects the
    /// envelope that would have gone over the wire. If the two ever
    /// disagree — a new integration adding a field, a sentry upgrade moving
    /// the context key — this is the one that notices.
    #[test]
    fn a_prompt_cannot_escape_through_a_tracing_event() {
        // Builder, not a struct literal: ClientOptions is #[non_exhaustive]
        // in 0.49 and a literal will not compile.
        let opts = sentry::ClientOptions::new().before_send(crate::scrub::before_send);
        let envelopes = sentry::test::with_captured_envelopes_options(
            || {
                sentry::capture_event(sentry::protocol::Event {
                    message: Some("boom".into()),
                    extra: [
                        ("session_id".to_string(), "abc-123".into()),
                        ("prompt".to_string(), "PINEAPPLE-42".into()),
                        ("git_token".to_string(), "ghp_secret".into()),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                });
            },
            opts,
        );

        let body = format!("{envelopes:?}");
        assert!(body.contains("abc-123"), "session_id must survive: {body}");
        assert!(!body.contains("PINEAPPLE-42"), "a prompt reached the wire: {body}");
        assert!(!body.contains("ghp_secret"), "a token reached the wire: {body}");
    }
}
