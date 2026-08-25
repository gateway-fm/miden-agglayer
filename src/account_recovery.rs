//! Runtime self-heal for the proxy's miden-client account store.
//!
//! ## Why
//!
//! The proxy's local miden-client sqlite (`store.sqlite3`) holds the live
//! commitment + storage + code for each infrastructure account in
//! `bridge_accounts.toml`. Two distinct failure modes lose or stale that
//! state and brick every subsequent submission:
//!
//!   - **`AccountDataNotFound`** — local row missing entirely. The bali
//!     production incident: every aggoracle `insertGlobalExitRoot` push
//!     rejected at `service.rs:396` with `eth_sendRawTransaction: ERR
//!     account data wasn't found for account id <id>`. Cause: a prior
//!     `--reset-miden-store` run, OOM-induced corruption, or upstream
//!     miden-client state churn.
//!
//!   - **`IncorrectAccountInitialCommitment`** — local commitment lags
//!     the live node's, so `submit_new_transaction` is rejected at the
//!     node with code 4 (`rpc/errors/node/transaction.rs:22-48`). Bali
//!     hit this BEFORE the row was ever lost.
//!
//! ## How
//!
//! Runtime, inline retry — NOT a startup brick. When a Miden submission
//! returns either of those errors, the caller invokes
//! [`reimport_account`] to fetch the latest state from the live Miden
//! node (via `Client::import_account_by_id`, which upstream calls
//! `add_account(overwrite=true)` and refreshes the local commitment),
//! then retries the submission once.
//!
//! ## Why NOT startup verification (the design we deleted)
//!
//! Not every account in `bridge_accounts.toml` is fully tracked by the
//! node at every moment. The locally-deployed `service` account
//! is created by `add_wallet` (`init.rs:125-153`) but never gets an
//! explicit `deploy_account` call — they exist locally until first use,
//! at which point `submit_new_transaction` deploys them on-chain. A
//! startup `verify_or_reimport_or_fail` call against those accounts
//! returns `AccountNotFoundOnChain` and bricks the proxy at boot, which
//! is wrong — those accounts are functionally healthy.
//!
//! The runtime approach fixes only what's actually broken when it's
//! actually broken, and the cost of one extra node RPC + one retry per
//! incident is well below the SLO impact of a CrashLoopBackoff.

use crate::accounts_config::AccountsConfig;
use crate::miden_client::MidenClient;
use miden_client::ClientError;
use miden_client::rpc::node::AddTransactionError;
use miden_client::rpc::{EndpointError, RpcError};
use miden_protocol::account::AccountId;

/// Returns `true` if the error chain contains either of the two account-state
/// errors that the runtime self-heal can recover.
///
/// Two reasons we walk the typed error chain instead of string-matching the
/// Display:
///
/// 1. The variant carrying the stale-commitment rejection has moved twice
///    already (0.15 `IncorrectAccountInitialCommitment` → 0.16.0-rc.1
///    `StateConflict { message }`). Typed matching breaks loudly at the compile
///    boundary when that happens; a string match silently stops firing.
///
/// 2. A miden-client upgrade that rewords a Display would silently disable our
///    retry.
///
/// The one place a substring is still unavoidable is *inside* the typed
/// `StateConflict` variant — see `NODE_ACCOUNT_COMMITMENT_MISMATCH` for why,
/// and for how that dependency is pinned by tests against real wire messages.
pub fn is_recoverable_account_error(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        // Direct ClientError::AccountDataNotFound
        if let Some(client_err) = e.downcast_ref::<ClientError>() {
            if matches!(client_err, ClientError::AccountDataNotFound(_)) {
                return true;
            }
            // RpcError chain: ClientError::RpcError(RpcError) wrapping the
            // node-side IncorrectAccountInitialCommitment.
            if let ClientError::RpcError(rpc_err) = client_err
                && rpc_error_is_incorrect_initial_commitment(rpc_err)
            {
                return true;
            }
        }
        // Bare RpcError in the chain (some call sites unwrap ClientError).
        if let Some(rpc_err) = e.downcast_ref::<RpcError>()
            && rpc_error_is_incorrect_initial_commitment(rpc_err)
        {
            return true;
        }
        // Last-resort string match — belt-and-braces for paths that surface the
        // error as a bare anyhow message (e.g. `service.rs` formats the
        // ClientError into a String before it reaches us), so no typed value
        // survives to downcast. Carries the rc.1 node wording as well as the
        // 0.15-era forms, because the two node versions render this rejection
        // completely differently and a rollback must stay covered. Pinned by
        // the `state_conflict_*` / `recoverable_error_*` tests below.
        let s = format!("{e}");
        s.contains("account data wasn't found")
            || s.contains(NODE_ACCOUNT_COMMITMENT_MISMATCH)
            || s.contains("incorrect account initial commitment")
            || s.contains("IncorrectAccountInitialCommitment")
    })
}

/// The exact sentence fragment the NODE emits for a stale-initial-commitment
/// rejection, quoted from
/// `miden-node/crates/block-producer/src/errors.rs::StateConflict`:
///
/// ```text
/// #[error("initial account commitment {expected} does not match the current \
///          commitment {current} for account {account}")]
/// AccountCommitmentMismatch { .. }
/// ```
///
/// It reaches us because the node's gRPC layer renders the WHOLE source chain
/// into the status message (`miden_node_utils::ErrorReport::as_report()`
/// appends `"\ncaused by: {source}"` for each source), so the wire message for
/// this rejection is:
///
/// ```text
/// transaction conflicts with current mempool state
/// caused by: initial account commitment 0x… does not match the current commitment 0x… for account 0x…
/// ```
///
/// Only the outer sentence is carried by the typed variant
/// (`AddTransactionError::StateConflict`, error code 2) — it is shared by all
/// four `StateConflict` cases (nullifiers already exist, output notes already
/// exist, unauthenticated notes missing, account-commitment mismatch), so the
/// variant ALONE cannot tell us which one fired.
const NODE_ACCOUNT_COMMITMENT_MISMATCH: &str = "does not match the current commitment";

fn rpc_error_is_incorrect_initial_commitment(rpc_err: &RpcError) -> bool {
    // 0.16.0-rc.1 collapsed the granular AddTransaction variants (the old
    // `IncorrectAccountInitialCommitment`) into `StateConflict { message }`.
    //
    // HOUSE RULE (typed detection only) — DELIBERATE, DOCUMENTED EXCEPTION.
    // Upstream discards the structured discriminant at the gRPC boundary: all
    // four `StateConflict` causes arrive as error code 2 with the cause carried
    // only as prose in `message`. Reimporting the account is the right response
    // to a commitment mismatch and the WRONG response to "nullifiers already
    // exist" (that one means the tx is a genuine double-spend), so the variant
    // alone is not actionable and a substring is the only discriminator the
    // wire format leaves us.
    //
    // The dependency is therefore pinned two ways:
    //   * `NODE_ACCOUNT_COMMITMENT_MISMATCH` quotes the upstream `#[error(...)]`
    //     literal, with the upstream path in its doc comment;
    //   * `state_conflict_*` tests below assert against the REAL rendered wire
    //     messages for all four causes — the mismatch one must match, the other
    //     three must not.
    // Upstream ask (issue #110): expose the `StateConflict` discriminant as a
    // structured field/sub-code so this can go back to a pure typed match.
    rpc_err
        .endpoint_error()
        .map(|endpoint_err| {
            matches!(
                endpoint_err,
                EndpointError::AddTransaction(AddTransactionError::StateConflict { message })
                    if message.contains(NODE_ACCOUNT_COMMITMENT_MISMATCH)
            )
        })
        .unwrap_or(false)
}

/// Force-refresh a single account from the Miden node into the proxy's
/// local sqlite. Upstream's `import_account_by_id` calls
/// `add_account(..., overwrite=true)` (`account/mod.rs:230,248`), so
/// this works whether the local row is missing OR present-but-stale.
///
/// Errors are mapped into anyhow with the original ClientError text so
/// callers can `is_recoverable_account_error` against a returned heal
/// failure (e.g., the account turns out to be Private — that surfaces
/// as `ClientError::AccountIsPrivate` and the heal cannot proceed).
pub async fn reimport_account(
    client: &MidenClient,
    account_id: AccountId,
    label: &'static str,
) -> anyhow::Result<()> {
    let result = client
        .with(move |client| {
            Box::new(async move {
                match client.import_account_by_id(account_id).await {
                    Ok(()) => Ok(()),
                    Err(err) => Err(anyhow::Error::msg(format!(
                        "import_account_by_id({account_id}) failed: {err}"
                    ))),
                }
            })
        })
        .await;
    match result {
        Ok(()) => {
            tracing::info!(account = label, account_id = %account_id, "reimported from node");
            metrics::counter!(
                "miden_account_reimport_total",
                "account" => label,
                "outcome" => "ok",
            )
            .increment(1);
            Ok(())
        }
        Err(err) => {
            tracing::warn!(
                account = label,
                account_id = %account_id,
                err = %err,
                "account reimport failed"
            );
            metrics::counter!(
                "miden_account_reimport_total",
                "account" => label,
                "outcome" => "failed",
            )
            .increment(1);
            Err(err)
        }
    }
}

/// Re-import every account in `bridge_accounts.toml`. Per-account
/// failures are logged but NOT propagated — callers want this to be
/// best-effort idempotent before retrying a submission. The locally-
/// only accounts (e.g. `service`) that aren't
/// network-deployed will fail here with `AccountNotFoundOnChain` and
/// that's fine: if the next submission succeeds, those accounts get
/// deployed implicitly by the tx.
pub async fn reimport_known_accounts(client: &MidenClient, accounts: &AccountsConfig) {
    let targets: Vec<(&'static str, AccountId)> = {
        let mut v = vec![
            ("service", accounts.service.0),
            ("bridge", accounts.bridge.0),
        ];
        if let Some(g) = &accounts.ger_manager {
            v.push(("ger_manager", g.0));
        }
        if let Some(f) = &accounts.faucet_eth {
            v.push(("faucet_eth", f.0));
        }
        if let Some(f) = &accounts.faucet_agg {
            v.push(("faucet_agg", f.0));
        }
        v
    };
    for (label, id) in targets {
        let _ = reimport_account(client, id, label).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Belt-and-braces string-fallback path — covers errors that propagate
    /// up as anyhow strings, not typed downcastable values. This is the
    /// shape `service.rs:396` actually surfaces today.
    #[test]
    fn recoverable_error_match_account_data_not_found_string() {
        let err = anyhow::Error::msg(
            "account data wasn't found for account id 0xe9a21e616d9ed59016d481c7001393",
        );
        assert!(is_recoverable_account_error(&err));
    }

    #[test]
    fn recoverable_error_match_incorrect_initial_commitment_pascalcase_string() {
        // The PascalCase variant name appears in some debug formatting
        // chains; keep the fallback resilient to both forms.
        let err = anyhow::Error::msg("submission rejected: IncorrectAccountInitialCommitment");
        assert!(is_recoverable_account_error(&err));
    }

    #[test]
    fn recoverable_error_match_incorrect_initial_commitment_lowercase_string() {
        // The upstream Display form is lowercase + spaces (see
        // miden-client `rpc/errors/node/transaction.rs:21-22`).
        // This is what the production proxy log actually contains.
        let err = anyhow::Error::msg("rpc error: (incorrect account initial commitment)");
        assert!(is_recoverable_account_error(&err));
    }

    #[test]
    fn recoverable_error_rejects_unrelated() {
        let err = anyhow::Error::msg("some other rpc error: connection refused");
        assert!(!is_recoverable_account_error(&err));
    }

    /// The REAL rc.1 wire message for a stale-initial-commitment rejection.
    ///
    /// Reproduced exactly as the node builds it: the block-producer's
    /// `MempoolSubmissionError::StateConflict` Display, then the source chain
    /// appended by `miden_node_utils::ErrorReport::as_report()` (which is what
    /// the `#[grpc(...)]`-generated `From<Error> for tonic::Status` sends).
    /// Sources: miden-node `crates/block-producer/src/errors.rs` (both enums),
    /// `crates/grpc-error-macro/src/lib.rs`, `crates/utils/src/lib.rs`.
    ///
    /// Do NOT "tidy" this into friendlier prose — a fabricated message makes
    /// every assertion below vacuous, which is exactly how this test was
    /// false-green before (it asserted on the 0.15 wording, which rc.1 never
    /// emits).
    const RC1_WIRE_COMMITMENT_MISMATCH: &str = "transaction conflicts with current mempool state\ncaused by: initial account commitment 0x9f6f0e2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6 does not match the current commitment 0x1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f809 for account 0xe9a21e616d9ed59016d481c7001393";

    /// The other three `StateConflict` causes, rendered the same way. None of
    /// them may trigger an account reimport: "nullifiers already exist" in
    /// particular is a genuine double-spend, and healing the account there
    /// would retry a transaction the node correctly refused.
    const RC1_WIRE_NULLIFIERS_EXIST: &str = "transaction conflicts with current mempool state\ncaused by: nullifiers already exist: [0x4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c]";
    const RC1_WIRE_OUTPUT_NOTES_EXIST: &str = "transaction conflicts with current mempool state\ncaused by: output notes already exist: [0x5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d]";
    const RC1_WIRE_UNAUTHENTICATED_MISSING: &str = "transaction conflicts with current mempool state\ncaused by: unauthenticated input notes are unknown: [0x6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e]";

    /// The node's gRPC error code for a state conflict (miden-node
    /// `MempoolSubmissionError` -> `AddTransactionError::from_code`).
    const NODE_STATE_CONFLICT_CODE: u8 = 2;

    /// Build the endpoint error the way PRODUCTION does: from the wire
    /// (code, message) pair through upstream's OWN decoder, instead of naming
    /// the variant ourselves.
    ///
    /// The decode step is part of what this module depends on — the node sends
    /// code 2 with the cause as prose, and `AddTransactionError::from_code` is
    /// what turns that into the variant the predicate matches. Constructing
    /// `StateConflict { .. }` by hand would keep passing even if upstream
    /// re-mapped code 2 elsewhere, which is exactly the silent break these
    /// tests exist to catch.
    fn state_conflict_rpc_error(message: &str) -> RpcError {
        use miden_client::rpc::{GrpcError, RpcEndpoint};
        let decoded = AddTransactionError::from_code(NODE_STATE_CONFLICT_CODE, message);
        assert!(
            matches!(decoded, AddTransactionError::StateConflict { .. }),
            "upstream no longer decodes node error code {NODE_STATE_CONFLICT_CODE} to \
             StateConflict (got {decoded:?}) — the recovery predicate matches a variant the node \
             no longer produces"
        );
        RpcError::RequestError {
            endpoint: RpcEndpoint::SubmitProvenTx,
            error_kind: GrpcError::InvalidArgument,
            endpoint_error: Some(EndpointError::AddTransaction(decoded)),
            source: None,
        }
    }

    /// Typed path, positive: the real commitment-mismatch payload heals.
    #[test]
    fn state_conflict_commitment_mismatch_is_recoverable() {
        let rpc_err = state_conflict_rpc_error(RC1_WIRE_COMMITMENT_MISMATCH);
        assert!(
            rpc_error_is_incorrect_initial_commitment(&rpc_err),
            "the real rc.1 AccountCommitmentMismatch wire message must be detected"
        );
        let anyhow_err = anyhow::Error::new(ClientError::RpcError(rpc_err));
        assert!(
            is_recoverable_account_error(&anyhow_err),
            "is_recoverable_account_error must catch it through ClientError::RpcError"
        );
    }

    /// Typed path, negative: the other three causes share error code 2 and the
    /// same outer sentence — none may be mistaken for a stale account.
    #[test]
    fn state_conflict_other_causes_are_not_recoverable() {
        for (label, wire) in [
            ("nullifiers already exist", RC1_WIRE_NULLIFIERS_EXIST),
            ("output notes already exist", RC1_WIRE_OUTPUT_NOTES_EXIST),
            (
                "unauthenticated notes missing",
                RC1_WIRE_UNAUTHENTICATED_MISSING,
            ),
        ] {
            let rpc_err = state_conflict_rpc_error(wire);
            assert!(
                !rpc_error_is_incorrect_initial_commitment(&rpc_err),
                "{label} is a state conflict but NOT a stale-commitment one — reimporting the account here would retry a correctly-rejected transaction"
            );
            let anyhow_err = anyhow::Error::new(ClientError::RpcError(rpc_err));
            assert!(
                !is_recoverable_account_error(&anyhow_err),
                "{label} must not be classified as recoverable through the anyhow chain either"
            );
        }
    }

    /// The pinned fragment must actually be a substring of the upstream
    /// `#[error(...)]` literal it quotes — guards against someone editing the
    /// constant into something the node never emits.
    #[test]
    fn pinned_fragment_matches_upstream_error_literal() {
        // Verbatim from miden-node crates/block-producer/src/errors.rs:
        //   #[error("initial account commitment {expected} does not match the
        //            current commitment {current} for account {account}")]
        let upstream_literal = "initial account commitment {expected} does not match the current commitment {current} for account {account}";
        assert!(
            upstream_literal.contains(NODE_ACCOUNT_COMMITMENT_MISMATCH),
            "NODE_ACCOUNT_COMMITMENT_MISMATCH must be a literal substring of the upstream error format string"
        );
    }

    /// The string-fallback path (errors that arrive as bare anyhow messages,
    /// with no typed value left to downcast) must cover the rc.1 wording too.
    #[test]
    fn string_fallback_covers_rc1_wire_message() {
        let err = anyhow::Error::msg(RC1_WIRE_COMMITMENT_MISMATCH.to_string());
        assert!(
            is_recoverable_account_error(&err),
            "the rc.1 wire message must be recognised even when it arrives as a bare string"
        );
        let unrelated = anyhow::Error::msg(RC1_WIRE_NULLIFIERS_EXIST.to_string());
        assert!(
            !is_recoverable_account_error(&unrelated),
            "an unrelated state conflict must not be recoverable through the string path"
        );
    }
}
