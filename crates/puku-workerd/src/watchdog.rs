//! VM watchdog: notices a VM that died or hung while its host stayed up.
//!
//! Without it a dead session VM looked like a quiet one: the outbox simply
//! stopped growing, nothing ever wrote the exit marker, and the session was
//! eventually *parked for idleness* -- a crash reported as a pause, with
//! nothing restarting it. A dead machine VM stayed `running` for ever.
//!
//! The probe runs `true` inside the guest. It answers only if the VMM is
//! alive and the guest agent is responsive, so it catches both a VMM that
//! exited and a guest that wedged. One missed probe proves nothing (a
//! guest pegged by a build can be slow); `MISSES_TO_CRASH` in a row does.

use std::time::Duration;

use crate::vm::{ExecRequest, Vm};

/// How often each running VM is probed.
pub const PROBE_EVERY: Duration = Duration::from_secs(15);
/// How long one probe may take before it counts as missed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive misses before the VM is declared crashed (~45 s).
pub const MISSES_TO_CRASH: u32 = 3;

/// One liveness probe.
pub async fn probe(vm: &dyn Vm) -> Result<(), String> {
    let req = ExecRequest { cmd: "true".into(), timeout: Some(PROBE_TIMEOUT), ..Default::default() };
    match tokio::time::timeout(PROBE_TIMEOUT + Duration::from_secs(2), vm.exec(req)).await {
        Err(_) => Err(format!("no answer within {}s", PROBE_TIMEOUT.as_secs())),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Ok(Ok(out)) if out.success() => Ok(()),
        Ok(Ok(out)) => Err(format!("the probe exited {}", out.code)),
    }
}

/// Counts consecutive misses.
#[derive(Debug, Default)]
pub struct Watchdog {
    misses: u32,
}

impl Watchdog {
    /// Record a probe result. `Some(reason)` once the VM counts as crashed.
    pub fn observe(&mut self, result: Result<(), String>) -> Option<String> {
        match result {
            Ok(()) => {
                self.misses = 0;
                None
            }
            Err(e) => {
                self.misses += 1;
                (self.misses >= MISSES_TO_CRASH).then(|| {
                    format!("the VM stopped answering ({} checks in a row): {e}", self.misses)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_slow_probe_is_not_a_crash_three_in_a_row_are() {
        let mut w = Watchdog::default();
        assert!(w.observe(Err("slow".into())).is_none());
        assert!(w.observe(Ok(())).is_none(), "an answer resets the count");
        assert!(w.observe(Err("x".into())).is_none());
        assert!(w.observe(Err("x".into())).is_none());
        let crashed = w.observe(Err("vmm gone".into())).expect("third miss in a row");
        assert!(crashed.contains("3 checks in a row") && crashed.contains("vmm gone"), "{crashed}");
    }

    #[tokio::test]
    async fn the_probe_fails_on_a_vm_that_does_not_answer() {
        let fake = crate::vm::fake::FakeBackend::new(puku_cloud_proto::Engine::Libkrun);
        let vm = crate::vm::VmBackend::create(&fake, &crate::vm::VmSpec::default()).await.unwrap();
        assert!(probe(vm.as_ref()).await.is_ok());
        fake.fail_execs(true);
        assert!(probe(vm.as_ref()).await.is_err());
    }
}
