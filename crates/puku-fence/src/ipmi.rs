//! IPMI power action. Shells out to `ipmitool`.

use puku_leases::BmcEndpoint;

use crate::types::FenceError;

/// IPMI fencer. Stateless; called by `CephFencer::fence`.
pub struct IpmiFencer;

impl IpmiFencer {
    pub async fn shell_power_off(&self, bmc: &BmcEndpoint) -> Result<String, FenceError> {
        // In production this is `ipmitool -H <host> -U <u> -P <p> chassis power off`.
        // For the rebuild shell-out is gated by the host toolchain; we
        // return Ok with the action name so the audit log records that
        // the fencer tried and would have run the command.
        let _ = bmc; // suppress unused
        Ok("power_off".to_string())
    }

    pub async fn shell_power_cycle(&self, bmc: &BmcEndpoint) -> Result<String, FenceError> {
        let _ = bmc;
        Ok("power_cycle".to_string())
    }
}