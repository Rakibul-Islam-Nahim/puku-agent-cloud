//! BMC reachability probe. Real impl talks to IPMI/Redfish; tests use a stub.

use async_trait::async_trait;

use crate::types::BmcEndpoint;

#[async_trait]
pub trait BmcProbe: Send + Sync {
    /// Is the BMC reachable right now? Implementations may cache for a few
    /// seconds to avoid hammering the controller.
    async fn is_reachable(&self, bmc: &BmcEndpoint) -> bool;
}

/// Always-reachable stub. Production wires IPMI/Redfish.
pub struct BmcProbeStub;

#[async_trait]
impl BmcProbe for BmcProbeStub {
    async fn is_reachable(&self, _bmc: &BmcEndpoint) -> bool {
        true
    }
}

/// Configurable stub for tests: returns the configured answer.
pub struct BmcProbeMock {
    pub answer: bool,
}

impl BmcProbeMock {
    pub fn new(answer: bool) -> Self {
        Self { answer }
    }
}

#[async_trait]
impl BmcProbe for BmcProbeMock {
    async fn is_reachable(&self, _bmc: &BmcEndpoint) -> bool {
        self.answer
    }
}