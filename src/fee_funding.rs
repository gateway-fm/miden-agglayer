//! Fee-asset funding for the proxy's on-chain accounts (#201).
//!
//! Protocol 0.16 charges `verification_base_fee × (ilog2(cycles) + 1)` on EVERY
//! transaction, paid by the EXECUTING account from ITS OWN vault in the chain's
//! native fee asset. Miden has no gas-paying EOA: a signing key only
//! *authorizes*; the account pays. So every account that executes — `service`,
//! `ger_manager`, the bridge, every faucet — needs the fee asset in its vault,
//! including the keyless ones that only the two KMS keys authorize.
//!
//! Model — fund only the KMS keys, cascade the rest:
//!   1. `--print-funding` builds + persists `service`/`ger_manager`, writes
//!      `funding.toml` (their addresses, the fee asset, recommended amounts) and
//!      exits without deploying.
//!   2. The operator funds JUST those two (fee asset sent as P2ID notes).
//!   3. `--init` resumes from the manifest. Each account's deploy CONSUMES its
//!      funding note — the vault is populated before the fee is deducted, so the
//!      deploy pays for itself — and `service` then cascade-funds the keyless
//!      bridge and faucets the same way. A zero-fee chain keeps the old
//!      empty-transaction deploy, unchanged.
//!
//! Custody is untouched: the cascade sends are signed by `service`'s (possibly
//! remote/KMS) key exactly like its claims. Minting the fee asset is NOT done
//! here — that is the operator's job (or, in e2e, `bridge-out-tool
//! --fund-fee-asset`, local custody by design).

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use miden_client::transaction::{PaymentNoteDescription, TransactionRequestBuilder};
use miden_protocol::MAX_TX_EXECUTION_CYCLES;
use miden_protocol::account::AccountId;
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{Note, NoteType};

use crate::accounts_config::AccountIdBech32;
use crate::metrics::ProofKind;
use crate::miden_client::MidenClientLib;

/// Transactions of fee headroom recommended per KMS-keyed account
/// (`ger_manager` pays on every GER injection, `service` on every claim).
pub const KMS_ACCOUNT_TXN_BUDGET: u64 = 256;
/// Fee headroom cascaded to each keyless account (bridge, faucet) at deploy.
/// Counted in worst-case (210-unit) transactions, but a bridge network
/// transaction really costs ~112 units, so 256 is ~480 of them: several hours
/// at a busy bridge's 27 per ten minutes — enough for a human to act on the
/// `txns_left` alert. 64 (≈120 transactions) ran a bridge dry in 2.5 h of e2e.
pub const CASCADE_TXN_BUDGET: u64 = 256;
/// Keyless accounts `--init` cascade-funds from `service`: the bridge and the
/// ETH faucet. Later faucets are cascade-funded at runtime as tokens appear.
pub const INIT_CASCADE_TARGETS: u64 = 2;
/// How many cascades `service` is funded for up front: the two init targets plus
/// runtime faucets (one per new token bridged in). Loadtest N=30 (9 new tokens)
/// ran `service` dry after four; the real answer for an unbounded token count is
/// the fee-vault monitor and top-ups, this just makes the common case not stall.
/// Env-tunable so the exhaustion e2e can leave `service` with only its own budget.
pub fn cascade_reserve_targets() -> u64 {
    budget("FEE_TXN_BUDGET_CASCADE_TARGETS", INIT_CASCADE_TARGETS + 10)
}
const FUNDING_POLL: Duration = Duration::from_secs(5);

/// The chain's fee parameters, read from the genesis header (constant for the
/// chain's lifetime; the sync-height header is the fallback, as in `fee_policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSnapshot {
    pub verification_base_fee: u32,
    pub fee_faucet_id: AccountId,
}

impl FeeSnapshot {
    pub fn charges_fees(&self) -> bool {
        self.verification_base_fee > 0
    }
}

pub async fn fee_snapshot(client: &mut MidenClientLib) -> anyhow::Result<FeeSnapshot> {
    for block in [BlockNumber::GENESIS, client.get_sync_height().await?] {
        if let Some((header, _)) = client.get_block_header_by_num(block).await? {
            let p = header.fee_parameters();
            return Ok(FeeSnapshot {
                verification_base_fee: p.verification_base_fee(),
                fee_faucet_id: p.fee_faucet_id(),
            });
        }
    }
    anyhow::bail!(
        "no block header available locally (genesis or sync height) — cannot read the chain \
         fee parameters; sync the client first"
    )
}

/// Worst-case fee of ONE transaction. The kernel charges
/// `verification_base_fee × (ilog2(total_cycles) + 1)` and cycles are bounded by
/// `MAX_TX_EXECUTION_CYCLES`, so this bounds any single transaction's fee.
pub fn max_fee_per_txn(verification_base_fee: u32) -> u64 {
    u64::from(verification_base_fee) * u64::from(MAX_TX_EXECUTION_CYCLES.ilog2() + 1)
}

/// A transaction budget, overridable per account through the environment so the
/// fee-exhaustion e2e can start an account nearly dry (`FEE_TXN_BUDGET_<NAME>`).
fn budget(env: &str, default: u64) -> u64 {
    std::env::var(env)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Recommended fee-asset funding for `ger_manager`.
pub fn recommended_ger_manager(fee: &FeeSnapshot) -> u64 {
    max_fee_per_txn(fee.verification_base_fee)
        * budget("FEE_TXN_BUDGET_GER_MANAGER", KMS_ACCOUNT_TXN_BUDGET)
}

/// Recommended fee-asset funding for `service`: its own budget PLUS everything
/// it cascades to the keyless accounts at init.
pub fn recommended_service(fee: &FeeSnapshot) -> u64 {
    let per = max_fee_per_txn(fee.verification_base_fee);
    per * budget("FEE_TXN_BUDGET_SERVICE", KMS_ACCOUNT_TXN_BUDGET)
        + cascade_amount(fee) * cascade_reserve_targets()
}

/// What `service` sends each keyless account it funds.
pub fn cascade_amount(fee: &FeeSnapshot) -> u64 {
    max_fee_per_txn(fee.verification_base_fee)
        * budget("FEE_TXN_BUDGET_CASCADE", CASCADE_TXN_BUDGET)
}

fn carries_fee_asset(note: &Note, fee: &FeeSnapshot) -> bool {
    note.assets()
        .iter()
        .any(|a| matches!(a, Asset::Fungible(f) if f.faucet_id() == fee.fee_faucet_id))
}

/// Poll until `account_id` has at least one consumable note carrying the fee
/// asset, syncing between polls. The first miss logs the exact funding
/// instruction so an operator watching the log knows what to send where.
/// Consume every pending fee-asset note addressed to `account_id` (a top-up).
/// Nobody else would: the ntx-builder only executes network notes, and a
/// wallet's proxy-side transactions never look for stray P2IDs. The consumed
/// note pays for its own consume, so this works on an empty vault too.
pub async fn consume_fee_notes(
    client: &mut MidenClientLib,
    account_id: AccountId,
    fee: &FeeSnapshot,
) -> anyhow::Result<usize> {
    let mut notes = Vec::new();
    for (record, _) in client.get_consumable_notes(Some(account_id)).await? {
        let Ok(note) = <_ as TryInto<Note>>::try_into(record) else {
            continue;
        };
        if carries_fee_asset(&note, fee) {
            notes.push(note);
        }
    }
    let n = notes.len();
    if n > 0 {
        let request = TransactionRequestBuilder::new().build_consume_notes(notes)?;
        crate::miden_client::submit_new_transaction(client, account_id, request).await?;
    }
    Ok(n)
}

pub async fn wait_for_funding_notes(
    client: &mut MidenClientLib,
    account_id: AccountId,
    name: &str,
    fee: &FeeSnapshot,
    wait: Duration,
) -> anyhow::Result<Vec<Note>> {
    let deadline = Instant::now() + wait;
    let mut announced = false;
    loop {
        client.sync_state().await?;
        let mut notes = Vec::new();
        for (record, _) in client.get_consumable_notes(Some(account_id)).await? {
            let note: Note = record
                .try_into()
                .map_err(|e| anyhow!("consumable note for {name} is not a full note: {e:?}"))?;
            if carries_fee_asset(&note, fee) {
                notes.push(note);
            }
        }
        if !notes.is_empty() {
            tracing::info!(
                account = name,
                account_id = %AccountIdBech32(account_id),
                notes = notes.len(),
                "fee-asset funding note(s) found; deploying by consuming them (#201)"
            );
            return Ok(notes);
        }
        let instruction = format!(
            "{name} account {} holds no fee-asset funding note. This chain charges fees \
             (verification_base_fee = {}); send it at least {} units of the native fee asset \
             {} as a P2ID note (see funding.toml / --print-funding, #201)",
            AccountIdBech32(account_id),
            fee.verification_base_fee,
            max_fee_per_txn(fee.verification_base_fee) * CASCADE_TXN_BUDGET,
            fee.fee_faucet_id.to_hex(),
        );
        if Instant::now() >= deadline {
            anyhow::bail!("{instruction}; gave up after {wait:?} (--funding-wait-secs)");
        }
        if !announced {
            tracing::warn!("{instruction}; waiting up to {wait:?}");
            announced = true;
        }
        tokio::time::sleep(FUNDING_POLL).await;
    }
}

/// `service` sends `amount` of the fee asset to `target` as a public P2ID note
/// — the cascade step. Signed by `service`'s key like any of its transactions.
pub async fn fund_from_service(
    client: &mut MidenClientLib,
    service_id: AccountId,
    target: AccountId,
    name: &str,
    amount: u64,
    fee: &FeeSnapshot,
) -> anyhow::Result<()> {
    let asset = Asset::Fungible(
        FungibleAsset::new(fee.fee_faucet_id, amount)
            .map_err(|e| anyhow!("fee asset {amount} for {name}: {e:?}"))?,
    );
    let request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![asset], service_id, target),
            NoteType::Public,
            client.rng(),
        )
        .map_err(|e| anyhow!("building fee-asset P2ID from service to {name}: {e:?}"))?;
    tracing::info!(
        target = name,
        target_id = %AccountIdBech32(target),
        amount,
        "cascade-funding from service (#201)"
    );
    let txn_id = crate::metrics::meter_proof(
        ProofKind::Init,
        crate::miden_client::submit_new_transaction(client, service_id, request),
    )
    .await
    .with_context(|| format!("service could not fund {name} (is service itself funded?)"))?;
    crate::miden_client::wait_for_transaction_commit(client, txn_id, 30, Duration::from_secs(2))
        .await?;
    client.sync_state().await?;
    Ok(())
}

/// What kind of account is being deployed — it decides who funds its
/// creation on a fee-charging chain. Both kinds are P2ID-fundable and deploy
/// by consuming that note (see `network_accounts` for why the bridge and the
/// faucets can be).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployKind {
    /// A `BasicWallet` (service, ger_manager): funded by the OPERATOR (the
    /// `fee-funder` in e2e); the deploy waits for that note.
    Wallet,
    /// A network account (bridge, faucet): `funder` (service) cascade-sends
    /// the P2ID first, then the deploy consumes it.
    NetworkAccount { funder: AccountId },
}

/// Deploy `account_id` on-chain.
///
/// Zero-fee chain: an empty transaction anchors the account (the historical
/// behaviour, unchanged, for every kind). Fee-charging chain: a wallet's deploy
/// CONSUMES its fee-asset funding note(s), so the very transaction that
/// deploys the account also funds it — for a network account after `funder`
/// cascade-sends that note (see [`DeployKind::NetworkAccount`]). An account
/// that is already
/// on-chain (a resumed `--init`) is skipped: its funding note was consumed by
/// the deploy that put it there.
pub async fn deploy_account(
    client: &mut MidenClientLib,
    account_id: AccountId,
    name: &str,
    kind: DeployKind,
    proof: ProofKind,
    fee: &FeeSnapshot,
    wait: Duration,
) -> anyhow::Result<()> {
    if fee.charges_fees() {
        // Resume: --init reuses the accounts funding.toml records. One that was
        // already deployed (nonce > 0) must not wait for a second funding note
        // that will never come — its note was consumed by the first deploy.
        client.sync_state().await?;
        if let Some(account) = client.get_account(account_id).await?
            && !account.is_new()
        {
            tracing::info!(
                account = name,
                account_id = %AccountIdBech32(account_id),
                nonce = %account.nonce(),
                "already deployed on-chain — skipping deploy (#201)"
            );
            return Ok(());
        }
        if let DeployKind::NetworkAccount { funder } = kind {
            fund_from_service(client, funder, account_id, name, cascade_amount(fee), fee).await?;
        }
    }
    tracing::info!(
        "deploying {} account {} ...",
        name,
        AccountIdBech32(account_id)
    );
    let request = if !fee.charges_fees() {
        TransactionRequestBuilder::new().build()?
    } else {
        let notes = wait_for_funding_notes(client, account_id, name, fee, wait).await?;
        TransactionRequestBuilder::new()
            .build_consume_notes(notes)
            .map_err(|e| anyhow!("building funding-note consumption for {name}: {e:?}"))?
    };
    let txn_id = crate::metrics::meter_proof(
        proof,
        crate::miden_client::submit_new_transaction(client, account_id, request),
    )
    .await?;
    tracing::info!("deployed {name} account with txn_id {txn_id}");

    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("deploy tx {txn_id} committed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fee(base: u32) -> FeeSnapshot {
        FeeSnapshot {
            verification_base_fee: base,
            fee_faucet_id: AccountId::from_hex("0x18101fa522c174b165efd4f70a0385").unwrap(),
        }
    }

    #[test]
    fn zero_base_fee_means_no_funding() {
        assert!(!fee(0).charges_fees());
        assert_eq!(max_fee_per_txn(0), 0);
        assert_eq!(recommended_service(&fee(0)), 0);
        assert_eq!(recommended_ger_manager(&fee(0)), 0);
    }

    /// The live testnet charges base fee 7. The kernel fee is
    /// `base × (ilog2(cycles)+1)`, bounded by MAX_TX_EXECUTION_CYCLES, so the
    /// per-txn bound is `7 × (ilog2(MAX)+1)` and the budgets scale from it.
    #[test]
    fn testnet_base_fee_7_amounts() {
        let f = fee(7);
        let per = 7 * u64::from(MAX_TX_EXECUTION_CYCLES.ilog2() + 1);
        assert_eq!(max_fee_per_txn(7), per);
        assert_eq!(recommended_ger_manager(&f), per * KMS_ACCOUNT_TXN_BUDGET);
        assert_eq!(cascade_amount(&f), per * CASCADE_TXN_BUDGET);
        assert_eq!(
            recommended_service(&f),
            per * KMS_ACCOUNT_TXN_BUDGET + per * CASCADE_TXN_BUDGET * cascade_reserve_targets()
        );
        // service must be able to fund every init cascade target out of its own
        // recommendation and still keep its full personal budget.
        assert!(
            recommended_service(&f)
                >= recommended_ger_manager(&f) + cascade_amount(&f) * cascade_reserve_targets()
        );
    }
}
