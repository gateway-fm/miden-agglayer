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
//! node at every moment. Locally-deployed `service` and `wallet_hardhat`
//! are created by `add_wallet` (`init.rs:125-153`) but never get an
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
/// 1. Upstream's `AddTransactionError::IncorrectAccountInitialCommitment` has
///    `#[error("incorrect account initial commitment")]` (lowercase, with
///    spaces — see miden-client `rpc/errors/node/transaction.rs:21-22`). A
///    PascalCase string-match never fires. Typed downcast is correct.
///
/// 2. A miden-client upgrade that rewords either message would silently
///    disable our retry. Typed matching breaks loudly at the compile boundary
///    when the variant moves or renames.
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
        // Last-resort string match — kept as a belt-and-braces fallback for
        // any error path we miss. Matches both the original PascalCase token
        // (in case a tool surfaces the enum variant name directly) AND the
        // lowercase Display form. Covered by tests.
        let s = format!("{e}");
        s.contains("account data wasn't found")
            || s.contains("incorrect account initial commitment")
            || s.contains("IncorrectAccountInitialCommitment")
    })
}

fn rpc_error_is_incorrect_initial_commitment(rpc_err: &RpcError) -> bool {
    rpc_err
        .endpoint_error()
        .map(|endpoint_err| {
            matches!(
                endpoint_err,
                EndpointError::AddTransaction(
                    AddTransactionError::IncorrectAccountInitialCommitment,
                )
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
/// only accounts (e.g. `wallet_hardhat`, `service`) that aren't
/// network-deployed will fail here with `AccountNotFoundOnChain` and
/// that's fine: if the next submission succeeds, those accounts get
/// deployed implicitly by the tx.
pub async fn reimport_known_accounts(client: &MidenClient, accounts: &AccountsConfig) {
    let targets: Vec<(&'static str, AccountId)> = {
        let mut v = vec![
            ("service", accounts.service.0),
            ("bridge", accounts.bridge.0),
            ("wallet_hardhat", accounts.wallet_hardhat.0),
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

    /// Typed downcast path — guards against an upstream rewording silently
    /// disabling the retry. If `AddTransactionError::IncorrectAccountInitialCommitment`
    /// ever moves or gets renamed, this stops compiling. The test relies on
    /// `rpc_error_is_incorrect_initial_commitment` walking a real `RpcError`
    /// constructed below.
    #[test]
    fn typed_downcast_catches_incorrect_initial_commitment() {
        use miden_client::rpc::{GrpcError, RpcEndpoint};
        let endpoint_err =
            EndpointError::AddTransaction(AddTransactionError::IncorrectAccountInitialCommitment);
        let rpc_err = RpcError::RequestError {
            endpoint: RpcEndpoint::SubmitProvenTx,
            error_kind: GrpcError::InvalidArgument,
            endpoint_error: Some(endpoint_err),
            source: None,
        };
        assert!(
            rpc_error_is_incorrect_initial_commitment(&rpc_err),
            "rpc_error_is_incorrect_initial_commitment must match the canonical AddTransaction(IncorrectAccountInitialCommitment) value"
        );

        let client_err = ClientError::RpcError(rpc_err);
        let anyhow_err = anyhow::Error::new(client_err);
        assert!(
            is_recoverable_account_error(&anyhow_err),
            "is_recoverable_account_error must catch RpcError-wrapped IncorrectAccountInitialCommitment via typed downcast"
        );
    }
}
