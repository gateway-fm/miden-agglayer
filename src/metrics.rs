use metrics::{describe_counter, describe_histogram};

pub fn init_metrics() {
    describe_counter!("rpc_requests_total", "Total JSON-RPC requests by method");
    describe_counter!("claims_processed_total", "Total claims processed");
    describe_counter!("ger_injections_total", "Total GER injections");
    describe_counter!("bridge_outs_total", "Total bridge-out operations");
    describe_counter!("store_errors_total", "Total store operation errors");
    describe_histogram!("rpc_request_duration_seconds", "JSON-RPC request duration");
}
