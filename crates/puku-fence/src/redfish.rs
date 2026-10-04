//! Redfish power action. HTTPS POST to the BMC's Redfish endpoint.

use puku_leases::BmcEndpoint;

use crate::types::FenceError;

pub struct RedfishFencer;

impl RedfishFencer {
    pub async fn shell_power_off(&self, bmc: &BmcEndpoint) -> Result<String, FenceError> {
        // Production: HTTPS POST to <host>/redfish/v1/Systems/.../Actions/ComputerSystem.Reset
        // with `{"ResetType": "ForceOff"}`. Body is built with the BMC
        // credentials (basic auth). For now we record the action.
        let _ = bmc;
        Ok("power_off".to_string())
    }

    pub async fn shell_power_cycle(&self, bmc: &BmcEndpoint) -> Result<String, FenceError> {
        let _ = bmc;
        Ok("power_cycle".to_string())
    }
}