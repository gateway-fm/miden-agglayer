use alloy::primitives::TxHash;
use alloy_core::sol_types::SolCall;
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};
use axum_jrpc::{JrpcResult, JsonRpcResponse};

// https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L71C19-L71C28
alloy_core::sol! {
    uint32 public networkID;
}

// https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L196
alloy_core::sol! {
    #[derive(Debug)]
    function bridgeAsset(
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        address token,
        bool forceUpdateGlobalExitRoot,
        bytes permitData
    );
}

#[repr(i32)]
pub(crate) enum ServiceErrorCode {
    SendRawTransaction = 1,
    GetTransactionReceipt,
    AdminRegisterFaucet,
    AdminRegisterNativeFaucet,
}

impl From<ServiceErrorCode> for JsonRpcErrorReason {
    fn from(value: ServiceErrorCode) -> Self {
        Self::ApplicationError(value as i32)
    }
}

/// Validate a hex-encoded 32-byte hash parameter from JSON-RPC requests.
///
/// Strips optional "0x" prefix, decodes hex, and verifies the result is exactly 32 bytes.
/// Returns a `JsonRpcResponse` error on failure (suitable for `?` in JrpcResult functions).
pub(crate) fn validate_hex_hash_param(
    hex_str: &str,
    field_name: &str,
    answer_id: axum_jrpc::Id,
) -> Result<[u8; 32], JsonRpcResponse> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(stripped).map_err(|_| {
        JsonRpcResponse::error(
            answer_id.clone(),
            JsonRpcError::new(
                JsonRpcErrorReason::InvalidParams,
                format!("bad {field_name}"),
                serde_json::Value::Null,
            ),
        )
    })?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        JsonRpcResponse::error(
            answer_id,
            JsonRpcError::new(
                JsonRpcErrorReason::InvalidParams,
                format!("{field_name} must be 32 bytes"),
                serde_json::Value::Null,
            ),
        )
    })?;
    Ok(arr)
}

/// Scrub an error chain for public consumption (R8).
///
/// `anyhow::Error` `Display` (and even more so `Debug` / chain) routinely contains
/// internal-only detail: filesystem paths from `Context::with_context`, hostnames
/// from network errors, sqlx schema names, miden-client store paths, etc.
/// Returning the raw chain to a JSON-RPC caller leaks those details to anyone
/// who can hit the port.
///
/// This helper:
/// 1. Logs the FULL error chain at `error` level so server-side observability
///    keeps everything (operators see the real cause in logs).
/// 2. Returns a redacted summary string suitable to send back to the caller —
///    keeps the high-level shape (e.g. "store error" / "claim error") but strips
///    paths, URLs, and bare punctuation that hints at internal layouts.
pub(crate) fn scrub_error(prefix: &str, e: &anyhow::Error) -> String {
    tracing::error!(target: "rpc::error", "{prefix}: {e:#}");
    redact_internal_details(&format!("{prefix}: {e}"))
}

/// Redact substrings that look like filesystem paths, file:line citations, or
/// URL prefixes from a public error message. Conservative — keeps the
/// human-readable summary but removes the load-bearing internal-leakage parts.
fn redact_internal_details(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        // Filesystem path heuristic: starts with `/` and contains another `/`.
        let looks_like_path = word.starts_with('/') && word[1..].contains('/');
        // URL heuristic.
        let looks_like_url = word.starts_with("http://")
            || word.starts_with("https://")
            || word.starts_with("postgres://")
            || word.starts_with("postgresql://");
        // Env-var-y: ALL_CAPS_WITH_UNDERSCORES of length >= 4.
        let looks_like_env =
            word.len() >= 4 && word.chars().all(|c| c.is_ascii_uppercase() || c == '_');

        if looks_like_path || looks_like_url || looks_like_env {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("<redacted>");
        } else {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    out
}

pub(crate) fn store_error(answer_id: axum_jrpc::Id, e: anyhow::Error) -> JsonRpcResponse {
    let message = scrub_error("store error", &e);
    JsonRpcResponse::error(
        answer_id,
        JsonRpcError::new(
            JsonRpcErrorReason::InternalError,
            message,
            serde_json::Value::Null,
        ),
    )
}

pub(crate) fn json_rpc_response_from_result<T: serde::Serialize>(
    result: anyhow::Result<T>,
    answer_id: axum_jrpc::Id,
    error_code: ServiceErrorCode,
) -> JrpcResult {
    match result {
        Ok(value) => Ok(JsonRpcResponse::success(answer_id, value)),
        Err(error) => {
            let message = scrub_error("rpc error", &error);
            let error = JsonRpcError::new(error_code.into(), message, serde_json::Value::Null);
            Err(JsonRpcResponse::error(answer_id, error))
        }
    }
}

/// SOAK FINDING #2 — render well-formed `claimAsset` calldata for a PROXY-SYNTHESIZED
/// claim transaction, reconstructed from its ClaimEvent log. Returns `None` for
/// non-ClaimEvent logs (or undecodable data), where the synthetic tx keeps its legacy
/// empty input.
///
/// aggkit's bridgesync (v0.8.3 L2BridgeSyncer) fetches EVERY claim's transaction by hash
/// and PARSES its `claimAsset` calldata ("DetailedClaimEvent"); a synthesized claim tx
/// serving `input: "0x"` fails its decoder with "input too short: 0 bytes", and the
/// downloader retries that block forever — wedging the whole certificate pipeline. The
/// ClaimEvent log data carries the exact fields the event was derived from
/// (`log_synthesis::encode_claim_event_data`: globalIndex | originNetwork |
/// originAddress | destinationAddress | amount, 5×32 bytes), so those are re-encoded
/// TRUTHFULLY; `destinationNetwork` is this rollup's network id (a claim served by this
/// chain is by definition destined for it). The SMT proofs / exit roots are NOT
/// recoverable from the log — they are zero-filled, keeping the calldata structurally
/// valid (correct selector + argument layout) for aggkit's parser, whose certificate
/// fields come from the truthful part.
///
/// Note tx bodies are NOT covered by the getLogs-immutability invariant: retroactively
/// serving calldata for an already-synthesized claim tx hash is safe (and is exactly
/// what un-wedges a live chain poisoned by an empty-input claim tx).
pub(crate) fn encode_claim_asset_from_log(
    log: &crate::log_synthesis::SyntheticLog,
    local_network_id: u32,
) -> Option<String> {
    if log.topics.first().map(String::as_str) != Some(crate::log_synthesis::CLAIM_EVENT_TOPIC) {
        return None;
    }
    let data_hex = log.data.strip_prefix("0x").unwrap_or(&log.data);
    let data = hex::decode(data_hex).ok()?;
    // ClaimEvent data layout (encode_claim_event_data): 5 fields × 32 bytes.
    if data.len() < 5 * 32 {
        return None;
    }
    let global_index = alloy::primitives::U256::from_be_slice(&data[0..32]);
    let origin_network = u32::from_be_bytes(data[60..64].try_into().ok()?);
    let origin_address: [u8; 20] = data[76..96].try_into().ok()?;
    let destination_address: [u8; 20] = data[108..128].try_into().ok()?;
    let amount = alloy::primitives::U256::from_be_slice(&data[128..160]);

    let zero32 = alloy::primitives::FixedBytes::<32>::ZERO;
    let call = crate::claim::claimAssetCall {
        smtProofLocalExitRoot: [zero32; 32],
        smtProofRollupExitRoot: [zero32; 32],
        globalIndex: global_index,
        mainnetExitRoot: zero32,
        rollupExitRoot: zero32,
        originNetwork: origin_network,
        originTokenAddress: alloy::primitives::Address::from(origin_address),
        destinationNetwork: local_network_id,
        destinationAddress: alloy::primitives::Address::from(destination_address),
        amount,
        metadata: alloy::primitives::Bytes::new(),
    };
    Some(format!("0x{}", hex::encode(SolCall::abi_encode(&call))))
}

pub(crate) fn build_synthetic_tx_json(
    txn_hash: TxHash,
    log: &crate::log_synthesis::SyntheticLog,
    chain_id: u64,
    local_network_id: u32,
) -> serde_json::Value {
    // SOAK FINDING #2: a ClaimEvent-bearing synthetic tx must serve parseable
    // `claimAsset` calldata (see `encode_claim_asset_from_log`); every other synthetic
    // tx keeps the legacy empty input.
    let input =
        encode_claim_asset_from_log(log, local_network_id).unwrap_or_else(|| "0x".to_string());
    serde_json::json!({
        "type": "0x0",
        "nonce": "0x0",
        "gasPrice": "0x0",
        "gas": "0x0",
        "to": &log.address,
        "value": "0x0",
        "input": input,
        "v": "0x1b",
        "r": "0x1",
        "s": "0x1",
        "hash": format!("{txn_hash:#x}"),
        "from": &log.address,
        "blockHash": format!("0x{}", hex::encode(log.block_hash)),
        "blockNumber": format!("0x{:x}", log.block_number),
        "transactionIndex": "0x0",
        "chainId": format!("0x{:x}", chain_id),
    })
}

/// RD-940 Decision 3 wire-shape — `eth_getTransactionByHash` JSON for a
/// **pending** (in-flight, not yet committed) transaction.
///
/// **Critical contract** (Spec D §2.4): only the three block-relative fields
/// are JSON `null` — `blockHash`, `blockNumber`, `transactionIndex`. All
/// other numeric fields must be hex strings, never `null`, because Go's
/// `hexutil.Uint{,64}` and `hexutil.Big` value-type unmarshallers panic on
/// `null`. aggkit's ethtxmanager treats the block-fields-null shape as
/// "accepted but not yet mined" and keeps polling; a single missing or
/// nulled non-block field is undetectable from Rust-only tests but breaks
/// aggkit silently.
///
/// Fields populated from the signed envelope (nonce, gas, value, to, input,
/// chainId); `from` is the recovered signer captured at enqueue time;
/// `v`/`r`/`s` are placeholder values matching `build_synthetic_tx_json` —
/// aggkit's monitor does not verify them.
pub(crate) fn build_inflight_pending_tx_json(
    entry: &crate::writer_worker::InFlightEntry,
    chain_id: u64,
) -> serde_json::Value {
    use alloy::consensus::TxEnvelope;
    use alloy::primitives::TxKind;

    // Pull the wire fields out of the signed envelope. Each variant exposes
    // the same surface but through a different concrete tx type — match
    // exhaustively so a future EIP-7702 / EIP-4844 path doesn't silently
    // emit zero-valued fields.
    let (nonce, gas, gas_price, value, to, input) = match &entry.envelope {
        TxEnvelope::Legacy(s) => {
            let t = s.tx();
            (
                t.nonce,
                t.gas_limit,
                t.gas_price,
                t.value,
                t.to,
                t.input.clone(),
            )
        }
        TxEnvelope::Eip1559(s) => {
            let t = s.tx();
            (
                t.nonce,
                t.gas_limit,
                t.max_fee_per_gas,
                t.value,
                t.to,
                t.input.clone(),
            )
        }
        TxEnvelope::Eip2930(s) => {
            let t = s.tx();
            (
                t.nonce,
                t.gas_limit,
                t.gas_price,
                t.value,
                t.to,
                t.input.clone(),
            )
        }
        TxEnvelope::Eip4844(s) => {
            let t = s.tx().tx();
            (
                t.nonce,
                t.gas_limit,
                t.max_fee_per_gas,
                t.value,
                TxKind::Call(t.to),
                t.input.clone(),
            )
        }
        TxEnvelope::Eip7702(s) => {
            let t = s.tx();
            (
                t.nonce,
                t.gas_limit,
                t.max_fee_per_gas,
                t.value,
                TxKind::Call(t.to),
                t.input.clone(),
            )
        }
    };

    let to_field = match to {
        TxKind::Call(addr) => serde_json::Value::String(format!("{addr:#x}")),
        TxKind::Create => serde_json::Value::Null,
    };

    serde_json::json!({
        "type": "0x0",
        "nonce": format!("0x{nonce:x}"),
        "gasPrice": format!("0x{gas_price:x}"),
        "gas": format!("0x{gas:x}"),
        "to": to_field,
        "value": format!("{value:#x}"),
        "input": format!("0x{}", ::hex::encode(input.as_ref())),
        // v/r/s placeholders — aggkit's monitor doesn't verify them on
        // pending; the actual signature lives on the envelope we already
        // accepted at sendRawTransaction time.
        "v": "0x1b",
        "r": "0x1",
        "s": "0x1",
        "hash": format!("{:#x}", entry.eth_tx_hash),
        "from": format!("{:#x}", entry.signer),
        // The three load-bearing nulls — pending tx, not yet mined.
        "blockHash": serde_json::Value::Null,
        "blockNumber": serde_json::Value::Null,
        "transactionIndex": serde_json::Value::Null,
        "chainId": format!("0x{chain_id:x}"),
    })
}

/// Encode `bridgeAsset(...)` calldata from a BridgeEvent synthetic log.
pub(crate) fn encode_bridge_asset_from_log(log: &crate::log_synthesis::SyntheticLog) -> String {
    let data_hex = log.data.strip_prefix("0x").unwrap_or(&log.data);
    let Ok(data_bytes) = hex::decode(data_hex) else {
        return "0x".to_string();
    };

    // BridgeEvent ABI field offsets (each field is padded to 32 bytes)
    const DEST_NET_OFFSET: usize = 3 * 32; // destinationNetwork
    const DEST_ADDR_OFFSET: usize = 4 * 32; // destinationAddress
    const AMOUNT_OFFSET: usize = 5 * 32; // amount
    const MIN_DATA_LEN: usize = 8 * 32;

    if data_bytes.len() < MIN_DATA_LEN {
        return "0x".to_string();
    }

    let dest_net = u32::from_be_bytes(
        data_bytes[DEST_NET_OFFSET + 28..DEST_NET_OFFSET + 32]
            .try_into()
            .unwrap_or([0; 4]),
    );
    let dest_addr: [u8; 20] = data_bytes[DEST_ADDR_OFFSET + 12..DEST_ADDR_OFFSET + 32]
        .try_into()
        .unwrap_or([0; 20]);
    let amount =
        alloy::primitives::U256::from_be_slice(&data_bytes[AMOUNT_OFFSET..AMOUNT_OFFSET + 32]);

    let call = bridgeAssetCall {
        destinationNetwork: dest_net,
        destinationAddress: alloy::primitives::Address::from(dest_addr),
        amount,
        token: alloy::primitives::Address::ZERO,
        forceUpdateGlobalExitRoot: false,
        permitData: alloy::primitives::Bytes::new(),
    };

    format!("0x{}", hex::encode(SolCall::abi_encode(&call)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_synthesis::SyntheticLog;

    /// Self-review R8 — repro+regression. Pre-fix, error chains from
    /// `anyhow::Error` Display flowed through `format!("store error: {e}")` and
    /// `error.to_string()` straight into JSON-RPC responses. Filesystem paths,
    /// URLs, and env var names embedded in `Context::with_context` chains
    /// landed in caller-visible payloads.
    ///
    /// Tests pin the redactor's behaviour:
    /// - filesystem paths replaced with `<redacted>`
    /// - URLs replaced with `<redacted>`
    /// - env-var-style ALL_CAPS strings replaced with `<redacted>`
    /// - normal English error text passes through unchanged
    #[test]
    fn r8_redact_internal_details_strips_paths_urls_envvars() {
        // Filesystem path
        let s = "failed to read /var/lib/miden-agglayer-service/keystore/key.bin";
        assert_eq!(
            redact_internal_details(s),
            "failed to read <redacted>",
            "filesystem path must be redacted"
        );

        // Multiple paths in one message
        let s = "io: src/store/db.rs:42 — cannot open /etc/foo /tmp/bar";
        let redacted = redact_internal_details(s);
        assert!(redacted.contains("io:"));
        assert!(!redacted.contains("/etc/foo"));
        assert!(!redacted.contains("/tmp/bar"));

        // Postgres URL
        let s = "connection refused to postgres://user:pass@host:5432/db";
        let redacted = redact_internal_details(s);
        assert!(!redacted.contains("postgres://"));
        assert!(!redacted.contains("user:pass"));
        assert!(redacted.contains("connection refused"));

        // Env var name
        let s = "missing DATABASE_URL env var";
        let redacted = redact_internal_details(s);
        assert!(!redacted.contains("DATABASE_URL"));
        assert!(redacted.contains("missing"));

        // Normal text passes through (no leak)
        let s = "claim amount must be positive";
        assert_eq!(redact_internal_details(s), "claim amount must be positive");

        // Single-letter words (e.g. variables) are kept; no over-redaction.
        let s = "value x is too small";
        assert_eq!(redact_internal_details(s), "value x is too small");
    }

    #[test]
    fn test_build_synthetic_tx_json_format() {
        let txn_hash = TxHash::from([5u8; 32]);
        let log = SyntheticLog {
            address: "0xc8cbebf950b9df44d987c8619f092bea980ff038".to_string(),
            topics: vec![],
            data: "0x".to_string(),
            block_number: 100,
            block_hash: [0xAA; 32],
            transaction_hash: format!("{txn_hash:#x}"),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };

        let json = build_synthetic_tx_json(txn_hash, &log, 2, 1);

        assert_eq!(json["type"], "0x0");
        assert_eq!(json["nonce"], "0x0");
        assert_eq!(json["gasPrice"], "0x0");
        assert_eq!(json["gas"], "0x0");
        assert_eq!(json["value"], "0x0");
        assert_eq!(json["input"], "0x");

        assert_eq!(json["from"], log.address);
        assert!(
            !json["blockHash"].is_null(),
            "blockHash must not be null for Go setSenderFromServer"
        );
        assert_eq!(json["blockNumber"], "0x64");
        assert_eq!(json["transactionIndex"], "0x0");

        assert_eq!(json["v"], "0x1b");
        assert_eq!(json["r"], "0x1");
        assert_eq!(json["s"], "0x1");

        assert_eq!(json["hash"], format!("{txn_hash:#x}"));
        assert_eq!(json["chainId"], "0x2");
    }

    #[test]
    fn test_build_synthetic_tx_json_different_blocks() {
        let txn_hash = TxHash::from([6u8; 32]);
        let log = SyntheticLog {
            address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
            topics: vec![],
            data: "0x".to_string(),
            block_number: 255,
            block_hash: [0xBB; 32],
            transaction_hash: "0xabc".to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };

        let json = build_synthetic_tx_json(txn_hash, &log, 1337, 1);

        assert_eq!(json["blockNumber"], "0xff");
        assert_eq!(json["chainId"], "0x539");
        assert_eq!(json["from"], log.address);
        assert_eq!(json["to"], log.address);
    }

    /// SOAK FINDING #2 regression — a PROXY-SYNTHESIZED claim tx (MA#27 chain-tail
    /// watcher / derived-hash path) must serve WELL-FORMED `claimAsset` calldata:
    /// correct selector, decodable argument layout, truthful globalIndex (+ origin/
    /// destination/amount, all straight from the ClaimEvent log the tx bears).
    /// aggkit v0.8.3's bridgesync parses this calldata for every claim; an empty
    /// input wedges its downloader ("input too short: 0 bytes") and halts certs.
    #[test]
    fn synthesized_claim_tx_serves_wellformed_claim_asset_calldata() {
        use alloy_core::sol_types::SolCall;
        let gi: [u8; 32] = {
            let mut b = [0u8; 32];
            b[24..].copy_from_slice(&0x8000000000000028u64.to_be_bytes()); // the soak gi
            b
        };
        let origin_addr = [0xABu8; 20];
        let dest_addr = [0xCDu8; 20];
        let data = crate::log_synthesis::encode_claim_event_data_u64(
            &gi,
            0, // origin network (L1 mainnet)
            &origin_addr,
            &dest_addr,
            1_000_000,
        );
        let txn_hash = TxHash::from([7u8; 32]);
        let log = SyntheticLog {
            address: "0xc8cbebf950b9df44d987c8619f092bea980ff038".to_string(),
            topics: vec![crate::log_synthesis::CLAIM_EVENT_TOPIC.to_string()],
            data,
            block_number: 8831, // the wedged soak block
            block_hash: [0xAA; 32],
            transaction_hash: format!("{txn_hash:#x}"),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        let local_network_id = 2u32;

        let json = build_synthetic_tx_json(txn_hash, &log, 1, local_network_id);
        let input = json["input"].as_str().expect("input is a string");
        assert_ne!(
            input, "0x",
            "a ClaimEvent-bearing tx must NOT serve empty calldata"
        );

        let raw = hex::decode(input.strip_prefix("0x").unwrap()).expect("valid hex");
        assert!(
            raw.starts_with(&crate::claim::claimAssetCall::SELECTOR),
            "calldata must carry the claimAsset selector"
        );
        let decoded =
            crate::claim::claimAssetCall::abi_decode(&raw).expect("aggkit-parseable layout");
        assert_eq!(
            decoded.globalIndex,
            alloy::primitives::U256::from_be_slice(&gi),
            "globalIndex must be truthful (aggkit derives the certificate from it)"
        );
        assert_eq!(decoded.originNetwork, 0);
        assert_eq!(decoded.originTokenAddress.as_slice(), &origin_addr);
        assert_eq!(decoded.destinationNetwork, local_network_id);
        assert_eq!(decoded.destinationAddress.as_slice(), &dest_addr);
        assert_eq!(decoded.amount, alloy::primitives::U256::from(1_000_000u64));
        assert!(
            decoded.metadata.is_empty(),
            "no metadata is reconstructable"
        );
    }

    /// EVERY ClaimEvent-bearing synthetic tx the service serves must have non-empty
    /// input, across data variants; non-claim synthetic txs keep the legacy empty
    /// input (bridge events are read from LOGS by aggkit, not calldata).
    #[test]
    fn every_claim_event_bearing_synthetic_tx_has_non_empty_input() {
        let mk_log = |topics: Vec<String>, data: String| SyntheticLog {
            address: "0xc8cbebf950b9df44d987c8619f092bea980ff038".to_string(),
            topics,
            data,
            block_number: 1,
            block_hash: [0u8; 32],
            transaction_hash: "0x11".to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        // Several ClaimEvent data shapes (different gi / networks / amounts).
        for (gi_byte, net, amount) in [(1u8, 0u32, 1u64), (0x80, 5, u64::MAX), (0xFF, 42, 0)] {
            let mut gi = [0u8; 32];
            gi[0] = gi_byte;
            let data = crate::log_synthesis::encode_claim_event_data_u64(
                &gi, net, &[1u8; 20], &[2u8; 20], amount,
            );
            let log = mk_log(
                vec![crate::log_synthesis::CLAIM_EVENT_TOPIC.to_string()],
                data,
            );
            let json = build_synthetic_tx_json(TxHash::from([9u8; 32]), &log, 1, 2);
            assert_ne!(
                json["input"], "0x",
                "ClaimEvent tx (gi_byte={gi_byte:#x}) must serve calldata"
            );
        }
        // Non-claim synthetic tx: unchanged legacy shape.
        let bridge_log = mk_log(vec!["0xdeadbeef".to_string()], "0x".to_string());
        let json = build_synthetic_tx_json(TxHash::from([9u8; 32]), &bridge_log, 1, 2);
        assert_eq!(
            json["input"], "0x",
            "non-claim synthetic txs keep empty input"
        );
    }
}
