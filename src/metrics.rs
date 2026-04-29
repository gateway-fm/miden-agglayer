use metrics::{describe_counter, describe_histogram};

pub fn init_metrics() {
    describe_counter!("rpc_requests_total", "Total JSON-RPC requests by method");
    describe_counter!("claims_processed_total", "Total claims processed");
    describe_counter!("ger_injections_total", "Total GER injections");
    describe_counter!("bridge_outs_total", "Total bridge-out operations");
    describe_counter!("store_errors_total", "Total store operation errors");
    describe_histogram!("rpc_request_duration_seconds", "JSON-RPC request duration");
    describe_counter!(
        "miden_client_build_errors_total",
        "Failed attempts to build Miden client connection"
    );
    describe_counter!(
        "miden_client_restarts_total",
        "Background thread restarts after crash"
    );
    describe_counter!("miden_sync_errors_total", "Sync errors by kind");
    describe_counter!(
        "bridge_out_self_targeted_total",
        "B2AGG bridge-outs whose destination_network equals our local network_id; \
         each one is a poison leaf that wedges the bridge (Cantina #13)"
    );
    describe_counter!(
        "bridge_let_divergence_total",
        "Local Exit Tree divergence events (Cantina #9). Labels: \
         kind=on_chain_ahead (private B2AGG was consumed) or \
         kind=aggkit_ahead (local state corruption)."
    );
    describe_counter!(
        "bridge_burn_serial_collision_total",
        "BURN note serial collisions (Cantina #5). Each increment marks \
         a BURN note whose serial number was already observed for a \
         different leaf — the bridge's `mint_and_send` token_supply is at \
         risk of exhaustion. Page critical."
    );
    describe_counter!(
        "bridge_twin_note_detected_total",
        "Twin-NoteId detections (Cantina #6). Each increment marks a \
         second on-chain note sharing a previously-observed NoteId but \
         differing in metadata — the B2AGG reclaim attack signature. \
         Page critical."
    );
    describe_counter!(
        "bridge_mint_target_mismatch_total",
        "MINT note consumed by a faucet other than its NetworkAccountTarget \
         attachment (Cantina #2). The claimant is about to receive the \
         wrong wrapped asset. Page critical."
    );
    describe_counter!(
        "bridge_faucet_ownership_drift_total",
        "Faucet owner storage slot has changed away from the configured \
         bridge AccountId (Cantina #4). Labels: kind=drift (transferred to \
         another account) or kind=renounced (owner cleared, faucet wedged). \
         Page critical."
    );
    describe_counter!(
        "bridge_forged_mint_total",
        "MINT note observed on-chain that does not correspond to any \
         aggkit-recorded claim (Cantina #4). Forged via NoAuth bridge \
         note authorship. Page critical, freeze claim processing."
    );
    describe_counter!(
        "bridge_expected_mint_stale_total",
        "Expected MINT NoteId did not land within the configured retry \
         threshold (Cantina #7). Indicates batch-dedup censorship via a \
         metadata-distinct twin. Triggers retry; alerts after K retries."
    );
}
