use crate::log_synthesis::LogFilter;
use crate::service_helpers::store_error;
use crate::service_state::ServiceState;
use axum_jrpc::{JrpcResult, JsonRpcExtractor, JsonRpcResponse};
use axum_jrpc::error::{JsonRpcError, JsonRpcErrorReason};

/// Maximum number of blocks an `eth_getLogs` request is allowed to span.
///
/// Self-review R5 — without a cap, an unauthenticated caller can request
/// `from_block=0x0, to_block=0xffffffffffffffff` and the store iterates the entire
/// 64-bit range looking for matches. Cap to a value that covers any realistic
/// indexer back-fill query but rejects obvious DoS attempts. 10_000 blocks ≈ 1 day at
/// 12s blocks; well-behaved consumers paginate.
pub const MAX_GETLOGS_BLOCK_RANGE: u64 = 10_000;

/// Maximum length of a `topics` filter array in an `eth_getLogs` request.
///
/// Per Ethereum spec, an event log carries at most 4 topics (topic0 = signature
/// hash + up to 3 indexed args). A filter with more than 4 entries cannot match
/// any real log, so accepting larger arrays is purely DoS surface. Initial
/// implementation set this to 256 — the security review flagged it as a 64×
/// over-allocation. Aligned to the spec.
pub const MAX_GETLOGS_TOPICS_LEN: usize = 4;

/// Maximum number of address-filter entries. Most consumers query 1-2 addresses
/// at a time; 32 is comfortable headroom for batched indexers.
pub const MAX_GETLOGS_ADDRESSES_LEN: usize = 32;

pub(crate) async fn service_get_logs(
    service: ServiceState,
    request: JsonRpcExtractor,
) -> JrpcResult {
    let answer_id = request.get_answer_id();
    let raw_params: (serde_json::Value,) = request.parse_params()?;
    // R7 — surface a malformed filter as InvalidParams instead of silently degrading
    // to "logs at the tip block". The previous fallback masked typos (e.g. an
    // unrecognised hex string) by returning an empty result for the wrong reason,
    // which downstream consumers would misinterpret as "no claims yet" and fail to
    // retry. JSON-RPC contract is to fail loud on bad input.
    let log_filter: LogFilter = serde_json::from_value(raw_params.0.clone()).map_err(|e| {
        tracing::warn!("eth_getLogs: rejecting malformed filter params: {e}");
        JsonRpcResponse::error(
            answer_id.clone(),
            JsonRpcError::new(
                JsonRpcErrorReason::InvalidParams,
                format!("invalid eth_getLogs filter: {e}"),
                serde_json::Value::Null,
            ),
        )
    })?;
    let current_block = service
        .store
        .get_latest_block_number()
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?;

    // R5 — bound the block range and filter array sizes before hitting the store.
    if let Err(msg) = validate_getlogs_filter(&log_filter, current_block) {
        return Ok(JsonRpcResponse::error(
            answer_id,
            JsonRpcError::new(
                JsonRpcErrorReason::InvalidParams,
                msg,
                serde_json::Value::Null,
            ),
        ));
    }

    let synthetic_logs = service
        .store
        .get_logs(&log_filter, current_block)
        .await
        .map_err(|e| store_error(answer_id.clone(), e))?;
    let json_logs: Vec<serde_json::Value> = synthetic_logs
        .iter()
        .map(|l: &crate::log_synthesis::SyntheticLog| l.to_json())
        .collect();

    Ok(JsonRpcResponse::success::<Vec<serde_json::Value>, _>(
        answer_id, json_logs,
    ))
}

/// Validate an eth_getLogs filter against the configured caps. Returns an error
/// message suitable for a JSON-RPC `InvalidParams` response.
pub fn validate_getlogs_filter(filter: &LogFilter, current_block: u64) -> Result<(), String> {
    let from = filter.from_block_number(current_block);
    let to = filter.to_block_number(current_block);
    // Saturate so a `from` that exceeds `to` doesn't underflow into a giant span.
    let span = to.saturating_sub(from).saturating_add(1);
    if span > MAX_GETLOGS_BLOCK_RANGE {
        return Err(format!(
            "eth_getLogs block range too large: {span} > {MAX_GETLOGS_BLOCK_RANGE} (paginate)"
        ));
    }
    #[allow(clippy::collapsible_if)]
    if let Some(topics) = filter.topics.as_ref() {
        if topics.len() > MAX_GETLOGS_TOPICS_LEN {
            return Err(format!(
                "eth_getLogs topics array too long: {} > {MAX_GETLOGS_TOPICS_LEN}",
                topics.len()
            ));
        }
    }
    if let Some(addresses) = filter.address.as_ref() {
        let len = match addresses {
            crate::log_synthesis::AddressFilter::Single(_) => 1,
            crate::log_synthesis::AddressFilter::Multiple(v) => v.len(),
        };
        if len > MAX_GETLOGS_ADDRESSES_LEN {
            return Err(format!(
                "eth_getLogs addresses array too long: {len} > {MAX_GETLOGS_ADDRESSES_LEN}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_synthesis::{AddressFilter, LogFilter, TopicFilter};

    /// Self-review R5 — repro+regression. Pre-fix, an unauthenticated caller could
    /// request a 64-bit block range and force the store to iterate billions of
    /// (mostly empty) blocks per request. Validate that the cap rejects oversized
    /// ranges and that legitimate-sized queries still pass.
    #[test]
    fn r5_eth_get_logs_block_range_capped() {
        // Rejects: full u64 range.
        let huge = LogFilter {
            from_block: Some("0x0".into()),
            to_block: Some("0xffffffffffffffff".into()),
            ..Default::default()
        };
        let err = validate_getlogs_filter(&huge, 100_000).unwrap_err();
        assert!(err.contains("block range too large"), "unexpected: {err}");

        // Rejects: 10_001 blocks (off-by-one).
        let just_over = LogFilter {
            from_block: Some("0x0".into()),
            to_block: Some(format!("0x{:x}", MAX_GETLOGS_BLOCK_RANGE)), // span = MAX + 1
            ..Default::default()
        };
        assert!(validate_getlogs_filter(&just_over, 1_000_000).is_err());

        // Accepts: exactly MAX blocks.
        let max = LogFilter {
            from_block: Some("0x1".into()),
            to_block: Some(format!("0x{:x}", MAX_GETLOGS_BLOCK_RANGE)),
            ..Default::default()
        };
        assert!(
            validate_getlogs_filter(&max, 1_000_000).is_ok(),
            "exact MAX should pass"
        );

        // Accepts: tip-only query (default `latest`/`latest` resolves to current_block).
        let tip = LogFilter::default();
        assert!(validate_getlogs_filter(&tip, 100).is_ok());
    }

    #[test]
    fn r5_eth_get_logs_topics_array_capped() {
        let too_many: Vec<Option<TopicFilter>> = (0..MAX_GETLOGS_TOPICS_LEN + 1)
            .map(|_| Some(TopicFilter::Single("0x00".into())))
            .collect();
        let f = LogFilter {
            from_block: Some("0x0".into()),
            to_block: Some("0x1".into()),
            topics: Some(too_many),
            ..Default::default()
        };
        let err = validate_getlogs_filter(&f, 100).unwrap_err();
        assert!(err.contains("topics array too long"), "unexpected: {err}");
    }

    /// Self-review R7 — repro+regression. Pre-fix, malformed filter JSON (e.g. a
    /// non-string `from_block` value, or an entirely invalid type) was silently
    /// degraded into `LogFilter::default()` which resolves to "logs at the tip
    /// block". Downstream consumers got an empty result for the wrong reason and
    /// would conclude "no claims" instead of retrying with a corrected filter.
    /// Post-fix the parse failure surfaces as JSON-RPC InvalidParams.
    ///
    /// We test the underlying serde behaviour because the actual handler requires
    /// a `JsonRpcExtractor` which is constructed from full HTTP request parts;
    /// a `from_value` round-trip is sufficient to pin the parse contract.
    #[test]
    fn r7_malformed_eth_get_logs_filter_is_rejected() {
        // `from_block` is declared as Option<String>; a numeric literal must NOT
        // be silently coerced into a default-tip filter.
        let bad = serde_json::json!({ "fromBlock": 12345 });
        assert!(
            serde_json::from_value::<LogFilter>(bad).is_err(),
            "numeric fromBlock must be rejected"
        );
        // Wholly invalid shape.
        let bad2 = serde_json::json!("not-an-object");
        assert!(serde_json::from_value::<LogFilter>(bad2).is_err());
        // Sane shape still parses.
        let good = serde_json::json!({
            "fromBlock": "0x0",
            "toBlock": "0x10",
        });
        assert!(serde_json::from_value::<LogFilter>(good).is_ok());
    }

    #[test]
    fn r5_eth_get_logs_addresses_array_capped() {
        let many: Vec<String> = (0..MAX_GETLOGS_ADDRESSES_LEN + 1)
            .map(|_| "0x0000000000000000000000000000000000000001".into())
            .collect();
        let f = LogFilter {
            from_block: Some("0x0".into()),
            to_block: Some("0x1".into()),
            address: Some(AddressFilter::Multiple(many)),
            ..Default::default()
        };
        let err = validate_getlogs_filter(&f, 100).unwrap_err();
        assert!(err.contains("addresses array too long"), "unexpected: {err}");
    }
}
