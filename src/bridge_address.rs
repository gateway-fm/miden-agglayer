//! Bridge contract address configuration.
//!
//! The bridge-service filters `eth_getLogs` by `L2PolygonBridgeAddresses`, so all synthetic
//! logs (ClaimEvent, BridgeEvent) must use the same address. This module provides a single
//! source of truth for that address, set once at startup via `init_bridge_address()`.

use std::sync::OnceLock;

/// Default bridge contract address (matches kurtosis-cdk deployment).
pub const DEFAULT_BRIDGE_ADDRESS: &str = "0xc8cbebf950b9df44d987c8619f092bea980ff038";

static BRIDGE_ADDRESS_CACHE: OnceLock<String> = OnceLock::new();

/// Initialize the bridge address. Must be called once at startup before any
/// service code calls `get_bridge_address()`.
pub fn init_bridge_address(address: String) {
    let _ = BRIDGE_ADDRESS_CACHE.set(address);
}

/// Returns the configured bridge contract address.
///
/// Falls back to `DEFAULT_BRIDGE_ADDRESS` if `init_bridge_address()` was not called
/// (e.g. in tests).
pub fn get_bridge_address() -> &'static str {
    BRIDGE_ADDRESS_CACHE.get_or_init(|| DEFAULT_BRIDGE_ADDRESS.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_bridge_address() {
        let addr = get_bridge_address();
        assert!(addr.starts_with("0x"));
        assert_eq!(addr.len(), 42);
    }
}
