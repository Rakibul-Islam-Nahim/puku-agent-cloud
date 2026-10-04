//! Keeping user content out of Sentry.
//!
//! Field values reach Sentry in four places: event contexts (sentry-tracing
//! writes every field into one keyed `"Rust Tracing Fields"`), breadcrumb
//! `data`, span and transaction `data`, and logs. `SessionSpec` carries
//! `prompt` and `git_token`, so one `tracing::error!(?spec, ..)` anywhere in
//! the tree would ship a customer's prompt and a GitHub token.
//!
//! Nothing does that today -- every existing field is a `session_id`,
//! `worker`, `path`, `key`, `status` or `error`. This module is about
//! staying that way, which is why it is a **whitelist**: a blacklist fails
//! open the first time someone writes `answer = %a`.

use std::sync::Arc;
use std::time::Duration;

use sentry::protocol::{Context, EnvelopeItem, Event, Map, Value};
use sentry::transports::DefaultTransportFactory;
use sentry::{Breadcrumb, Envelope, Transport, TransportFactory, TransportOptions};

/// The only field names allowed to leave the process inside an event.
///
/// Adding a name here is a deliberate act. Anything absent is replaced with
/// `[redacted]`, so a new `tracing::error!(prompt = %p, ..)` is safe by
/// default rather than dangerous by default.
pub const ALLOWED_FIELDS: &[&str] = &[
    // identity and routing
    "session_id",
    "org_id",
    "user_id",
    "worker",
    "worker_name",
    "instance_id",
    "schedule_id",
    "trigger_id",
    "sandbox_name",
    "image",
    "task",
    "service",
    // lifecycle
    "state",
    "status",
    "code",
    "attempt",
    "recovered",
    "kind",
    // diagnostics
    "error",
    "error_type",
    "duration_ms",
    "count",
    "len",
    "bytes",
    // callsite metadata sentry-tracing adds itself
    "sentry.tracing.target",
    "code.module.name",
    "code.file.path",
    "code.line.number",
    // reliability-rebuild (R6): metadata about the recovery system, not user content.
    // These identify WHICH component failed; none of them carry a payload.
    "host_id",
    "recovery_mode",
    "sla_tier",
    "tier",
    "fence_status",
    "lease_state",
    "snapshot_status",
    "manifest_id",
    "snapshot_id",
    "miss_count",
    "generation",
];

const REDACTED: &str = "[redacted]";

fn scrub_map(map: &mut Map<String, Value>) {
    for (k, v) in map.iter_mut() {
        if !ALLOWED_FIELDS.contains(&k.as_str()) {
            *v = Value::String(REDACTED.into());
        }
    }
}

/// Bound free text we allow through but cannot fully vouch for.
///
/// `error = format!("{e:#}")` on an anyhow chain can pick up a repo URL or a
/// filesystem path from a `.context(..)` call somewhere. A cap limits how
/// much of anything escapes.
fn cap(s: &mut String, n: usize) {
    if s.len() > n {
        s.truncate(n);
        s.push_str("…[truncated]");
    }
}

pub(crate) fn before_send(mut event: Event<'static>) -> Option<Event<'static>> {
    // sentry-tower already drops Authorization/Cookie/X-Api-Key and never
    // sets a body. This is the backstop for any future integration that is
    // less careful.
    if let Some(req) = event.request.as_mut() {
        req.data = None;
        req.cookies = None;
        req.query_string = None;
        req.env.clear();
        req.headers.retain(|k, _| {
            matches!(
                k.to_ascii_lowercase().as_str(),
                "content-type" | "content-length" | "user-agent" | "accept"
            )
        });
        if let Some(url) = req.url.as_mut() {
            url.set_query(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
        }
    }

    if let Some(Context::Other(fields)) = event.contexts.get_mut("Rust Tracing Fields") {
        scrub_map(fields);
    }
    scrub_map(&mut event.extra);

    for bc in event.breadcrumbs.values.iter_mut() {
        scrub_map(&mut bc.data);
        if let Some(msg) = bc.message.as_mut() {
            cap(msg, 512);
        }
    }
    for exc in event.exception.values.iter_mut() {
        if let Some(v) = exc.value.as_mut() {
            cap(v, 2048);
        }
    }
    if let Some(msg) = event.message.as_mut() {
        cap(msg, 2048);
    }

    // send_default_pii(false) does not guarantee a future integration will
    // not set one.
    event.user = None;
    Some(event)
}

pub(crate) fn before_breadcrumb(mut bc: Breadcrumb) -> Option<Breadcrumb> {
    scrub_map(&mut bc.data);
    Some(bc)
}

/// Scrubs transactions on their way out.
///
/// `before_send` covers events only. Transactions go straight from
/// `finish()` to `Transport::send_envelope` -- sentry-rust has never had a
/// `before_send_transaction` hook -- so the transport is the last place a
/// span field can be caught.
pub(crate) struct ScrubbingTransportFactory;

impl TransportFactory for ScrubbingTransportFactory {
    fn create_transport_with_options(&self, options: TransportOptions) -> Arc<dyn Transport> {
        Arc::new(ScrubbingTransport {
            inner: DefaultTransportFactory.create_transport_with_options(options),
        })
    }
}

struct ScrubbingTransport {
    inner: Arc<dyn Transport>,
}

impl Transport for ScrubbingTransport {
    fn send_envelope(&self, envelope: Envelope) {
        // Keep the headers: they carry the event id and the dynamic
        // sampling context. Envelope::new() would drop both.
        let headers = envelope.headers().clone();
        let mut out = Envelope::new().with_headers(headers);

        for item in envelope.into_items() {
            match item {
                EnvelopeItem::Transaction(mut tx) => {
                    scrub_map(&mut tx.extra);
                    tx.user = None;
                    tx.request = None;
                    for span in tx.spans.iter_mut() {
                        scrub_map(&mut span.data);
                        span.description = None;
                    }
                    out.add_item(EnvelopeItem::Transaction(tx));
                }
                other => out.add_item(other),
            }
        }
        self.inner.send_envelope(out);
    }

    fn flush(&self, timeout: Duration) -> bool {
        self.inner.flush(timeout)
    }

    fn shutdown(&self, timeout: Duration) -> bool {
        self.inner.shutdown(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    /// The whole privacy argument rests on this one behaviour.
    #[test]
    fn an_unlisted_field_is_redacted_and_a_listed_one_survives() {
        let mut m = fields(&[("session_id", "abc-123"), ("prompt", "PINEAPPLE-42")]);
        scrub_map(&mut m);
        assert_eq!(m["session_id"], Value::String("abc-123".into()));
        assert_eq!(m["prompt"], Value::String(REDACTED.into()));
    }

    /// A blacklist would have to guess every name someone might invent.
    /// This asserts the default is safe for names nobody thought of.
    #[test]
    fn fields_nobody_anticipated_are_redacted_by_default() {
        let mut m = fields(&[
            ("answer", "staging"),
            ("git_token", "ghp_secret"),
            ("spec", "prompt=hello"),
            ("some_future_field", "whatever"),
        ]);
        scrub_map(&mut m);
        for k in ["answer", "git_token", "spec", "some_future_field"] {
            assert_eq!(m[k], Value::String(REDACTED.into()), "{k} leaked");
        }
    }

    #[test]
    fn free_text_is_bounded() {
        let mut s = "x".repeat(5000);
        cap(&mut s, 2048);
        assert!(s.len() < 2100, "unbounded: {}", s.len());
        assert!(s.ends_with("[truncated]"));
    }

    #[test]
    fn short_text_is_left_alone() {
        let mut s = "boot failed".to_string();
        cap(&mut s, 2048);
        assert_eq!(s, "boot failed");
    }

    /// Every allowed name should be one a reader can justify; a duplicate
    /// suggests the list was edited carelessly.
    #[test]
    fn the_whitelist_has_no_duplicates() {
        let mut seen = ALLOWED_FIELDS.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate entry in ALLOWED_FIELDS");
    }
}
