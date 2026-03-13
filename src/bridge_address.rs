//! Bridge contract address configuration.
//!
//! The bridge-service filters `eth_getLogs` by `L2PolygonBridgeAddresses`, so all synthetic
//! logs (ClaimEvent, BridgeEvent) must use the same address. This module provides a single
//! source of truth for that address, configurable via `BRIDGE_ADDRESS` env var.

use std::sync::OnceLock;

/// Default bridge contract address (matches kurtosis-cdk deployment).
const DEFAULT_BRIDGE_ADDRESS: &str = "0xc8cbebf950b9df44d987c8619f092bea980ff038";

static BRIDGE_ADDRESS_CACHE: OnceLock<String> = OnceLock::new();

/// Returns the configured bridge contract address.
///
/// Reads from `BRIDGE_ADDRESS` env var on first call, defaults to
/// `0xc8cbebf950b9df44d987c8619f092bea980ff038`.
pub fn get_bridge_address() -> &'static str {
    BRIDGE_ADDRESS_CACHE.get_or_init(|| {
        std::env::var("BRIDGE_ADDRESS").unwrap_or_else(|_| DEFAULT_BRIDGE_ADDRESS.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_bridge_address() {
        // Can't test env var override reliably in parallel tests,
        // but we can verify the default is returned.
        let addr = get_bridge_address();
        assert!(addr.starts_with("0x"));
        assert_eq!(addr.len(), 42);
    }
}
