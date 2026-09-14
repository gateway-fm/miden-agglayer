//! Fee-vault monitor (#201): exports the native fee-asset balance of every
//! account the proxy depends on, so an operator can alert before one hits 0.
//!
//! On a fee-charging chain every account pays its own transaction fees from
//! its vault — `service` on every claim, `ger_manager` on every GER injection,
//! the bridge on every network transaction (UpdateGer, CLAIM, B2AGG…), each
//! faucet on every MINT/BURN. An empty vault silently stalls that account
//! (its transactions abort in the kernel). Measured at base fee 7: network
//! transactions cost ~105–112 units (the bridge burned 13,440 in 120 of them),
//! signed client transactions the 210 cap; a busy bridge ran 27 network
//! transactions in ten minutes — so the cascade amounts are hours, not weeks,
//! of runway, and the balance has to be watched.
//!
//! Gauges (labelled `account="service"|"ger_manager"|"bridge"|"faucet:<SYMBOL>"`):
//! - `bridge_fee_vault_balance` — units of the fee asset in the vault;
//! - `bridge_fee_vault_txns_left` — balance ÷ the per-transaction fee cap
//!   (`verification_base_fee × (ilog2(MAX_TX_EXECUTION_CYCLES)+1)`): a
//!   conservative "transactions before empty"; alert on this one;
//! - `bridge_fee_max_per_txn` — that cap (0 on a zero-fee chain, where the
//!   monitor reports once and idles).
//!
//! Logs WARN when an account has fewer than `warn_txns` transactions left and
//! ERROR at 0. Refreshes deployed network accounts from the node and sweeps
//! wallet top-ups before reading balances.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use miden_protocol::account::{Account, AccountId};
use miden_protocol::asset::Asset;
use tokio::sync::oneshot;

use crate::fee_funding::{FeeSnapshot, fee_snapshot, max_fee_per_txn};
use crate::miden_client::{MidenClient, MidenClientLib};
use crate::store::Store;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Default WARN threshold: transactions left at the fee cap.
pub const DEFAULT_WARN_TXNS: u64 = 32;

/// Fee-asset balance of `account`'s vault, in units (0 when absent).
pub fn fee_balance(account: &Account, fee_faucet_id: AccountId) -> u64 {
    account
        .vault()
        .assets()
        .find_map(|asset| match asset {
            Asset::Fungible(f) if f.faucet_id() == fee_faucet_id => Some(f.amount().as_u64()),
            _ => None,
        })
        .unwrap_or(0)
}

/// Conservative "transactions before empty" at the per-transaction fee cap.
pub fn txns_left(balance: u64, max_fee: u64) -> u64 {
    balance.checked_div(max_fee).unwrap_or(u64::MAX)
}

/// Refresh the balance independently of the proxy's transaction/sync activity.
async fn poll_account(
    client: &mut MidenClientLib,
    name: &str,
    id: AccountId,
    snap: &FeeSnapshot,
) -> anyhow::Result<Option<u64>> {
    let Some(account) = client.get_account(id).await? else {
        tracing::warn!(account = %name, id = %id.to_hex(), "fee-vault monitor: account not in the client store");
        return Ok(None);
    };
    // A new account's cascade note belongs to its deployment procedure, and
    // there is no on-chain state to import yet.
    if account.is_new() {
        return Ok(Some(fee_balance(&account, snap.fee_faucet_id)));
    }
    if miden_standards::account::auth::NetworkAccount::new(account).is_ok() {
        // get_account() only reads the local store. External credits to the
        // bridge/faucets can remain absent there even across syncs and GER
        // injections, so fetch the node's full state on every monitor tick.
        // Only network accounts are overwritten: wallets may have pending
        // local transactions that are ahead of the node.
        client.import_account_by_id(id).await?;
    } else {
        // The node refuses user-submitted transactions for deployed network
        // accounts; their top-ups are consumed by the ntx-builder. Wallets
        // need the proxy to sweep their fee-asset P2IDs.
        match crate::fee_funding::consume_fee_notes(client, id, snap).await {
            Ok(n) if n > 0 => {
                tracing::info!(account = %name, notes = n, "consuming fee-asset top-up note(s) (#201)")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(account = %name, error = %format!("{e:#}"), "could not consume a fee-asset top-up")
            }
        }
    }
    // Account is an owned snapshot, not a live handle: re-read after either
    // the node refresh or the wallet sweep, including its local fee delta.
    let account = client
        .get_account(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("monitored account {} disappeared", id.to_hex()))?;
    Ok(Some(fee_balance(&account, snap.fee_faucet_id)))
}

pub struct FeeVaultMonitor {
    miden_client: Arc<MidenClient>,
    store: Arc<dyn Store>,
    service: AccountId,
    ger_manager: Option<AccountId>,
    bridge: AccountId,
    poll_interval: Duration,
    warn_txns: u64,
    /// Last observed balance per account, to log what was withdrawn between ticks.
    last: Arc<Mutex<HashMap<String, u64>>>,
}

impl FeeVaultMonitor {
    pub fn new(
        miden_client: Arc<MidenClient>,
        store: Arc<dyn Store>,
        service: AccountId,
        ger_manager: Option<AccountId>,
        bridge: AccountId,
    ) -> Self {
        Self {
            miden_client,
            store,
            service,
            ger_manager,
            bridge,
            poll_interval: DEFAULT_POLL_INTERVAL,
            warn_txns: DEFAULT_WARN_TXNS,
            last: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn with_warn_txns(mut self, warn_txns: u64) -> Self {
        self.warn_txns = warn_txns;
        self
    }

    /// Spawn as a tokio task; returns a oneshot sender for graceful shutdown.
    pub fn spawn(self) -> oneshot::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                match self.tick().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!("fee-vault monitor: zero-fee chain — nothing to watch");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "fee-vault monitor tick failed")
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(self.poll_interval) => {}
                    _ = &mut shutdown_rx => return,
                }
            }
        });
        shutdown_tx
    }

    /// One pass. Returns `Ok(false)` on a zero-fee chain (nothing to monitor).
    async fn tick(&self) -> anyhow::Result<bool> {
        let mut targets: Vec<(String, AccountId)> = vec![("service".to_string(), self.service)];
        if let Some(ger) = self.ger_manager {
            targets.push(("ger_manager".to_string(), ger));
        }
        targets.push(("bridge".to_string(), self.bridge));
        for f in self.store.list_faucets().await? {
            targets.push((format!("faucet:{}", f.symbol), f.faucet_id));
        }
        let warn_txns = self.warn_txns;
        let last = self.last.clone();
        let charges = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let charges_inner = charges.clone();
        self.miden_client
            .with(move |client| {
                Box::new(async move {
                    let snap = fee_snapshot(client).await?;
                    let max_fee = max_fee_per_txn(snap.verification_base_fee);
                    metrics::gauge!("bridge_fee_max_per_txn").set(max_fee as f64);
                    if !snap.charges_fees() {
                        charges_inner.store(false, std::sync::atomic::Ordering::Relaxed);
                        return Ok(());
                    }
                    for (name, id) in targets {
                        let balance = match poll_account(client, &name, id, &snap).await {
                            Ok(Some(balance)) => balance,
                            Ok(None) => continue,
                            Err(e) => {
                                tracing::warn!(account = %name, id = %id.to_hex(), error = %format!("{e:#}"), "fee-vault monitor: could not refresh balance");
                                continue;
                            }
                        };
                        let left = txns_left(balance, max_fee);
                        metrics::gauge!("bridge_fee_vault_balance", "account" => name.clone())
                            .set(balance as f64);
                        metrics::gauge!("bridge_fee_vault_txns_left", "account" => name.clone())
                            .set(left as f64);
                        // Where did fees go: log every decrease since the last tick
                        // (an increase is a top-up / cascade landing).
                        let prev = last.lock().expect("fee-vault last-balance map").insert(name.clone(), balance);
                        match prev {
                            Some(p) if p > balance => tracing::info!(account = %name, spent = p - balance, balance, txns_left = left,
                                "fee vault spent since last check (#201)"),
                            Some(p) if p < balance => tracing::info!(account = %name, received = balance - p, balance,
                                "fee vault topped up (#201)"),
                            _ => {}
                        }
                        // Same threshold as the gauge operators alert on: below one
                        // worst-case fee the next transaction can already abort (the
                        // bridge stalled at 77 units, a 105-unit note in the queue), so
                        // "empty" is txns_left == 0, not a literal zero balance.
                        if left == 0 {
                            tracing::error!(account = %name, id = %id.to_hex(), balance,
                                "fee vault EMPTY — every transaction of this account will abort until it is topped up (#201)");
                        } else if name == "service"
                            && balance < crate::fee_funding::cascade_amount(&snap) + max_fee
                        {
                            // A cascade is a 64-tx-sized spend: service can look healthy per
                            // transaction and still be unable to fund the next new faucet.
                            tracing::warn!(account = %name, id = %id.to_hex(), balance,
                                "fee vault too low to fund another faucet — the next new token's claims will fail until topped up (#201)");
                        } else if left < warn_txns {
                            tracing::warn!(account = %name, id = %id.to_hex(), balance, txns_left = left,
                                "fee vault low — top up with a P2ID of the fee asset (#201)");
                        }
                    }
                    Ok(())
                })
            })
            .await?;
        Ok(charges.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_client::testing::account_id::ACCOUNT_ID_FEE_FAUCET;
    use miden_client::testing::mock::MockRpcApi;
    use miden_client::testing::{Auth, MockChainBuilder};
    use miden_protocol::Felt;
    use miden_protocol::asset::FungibleAsset;
    use miden_standards::account::auth::NetworkAccount;
    use miden_standards::account::wallets::BasicWallet;
    use miden_standards::note::P2idNote;

    fn snapshot() -> FeeSnapshot {
        FeeSnapshot {
            verification_base_fee: 7,
            fee_faucet_id: ACCOUNT_ID_FEE_FAUCET.try_into().unwrap(),
        }
    }

    fn network_account() -> Account {
        let allowed = std::collections::BTreeSet::from([P2idNote::script_root()]);
        let policy = crate::fee_policy::zero_fee_policy_manager_for(
            allowed.clone(),
            snapshot().fee_faucet_id,
        );
        NetworkAccount::builder([7; 32], allowed, policy)
            .unwrap()
            .with_component(BasicWallet)
            .build()
            .unwrap()
    }

    fn credit(account: &mut Account, amount: u64) {
        account
            .vault_mut()
            .add_asset(
                FungibleAsset::new(snapshot().fee_faucet_id, amount)
                    .unwrap()
                    .into(),
            )
            .unwrap();
        account.increment_nonce(Felt::ONE).unwrap();
    }

    async fn client_with_node_account(account: Account) -> MidenClientLib {
        let mut client = crate::test_helpers::offline_miden_client_lib().await;
        *client.test_rpc_api() = Arc::new(MockRpcApi::new(
            MockChainBuilder::with_accounts([account])
                .unwrap()
                .build()
                .unwrap(),
        ));
        client
    }

    /// PRST-4808: the node consumed the bridge's P2ID, but the tracked account
    /// still held 53,319. No proxy transaction or background sync may be needed
    /// for the next balance/runway sample to reflect that external credit.
    #[tokio::test]
    async fn network_top_up_refreshes_balance_without_proxy_activity() {
        let mut cached = network_account();
        credit(&mut cached, 53_319);
        let mut on_chain = cached.clone();
        credit(&mut on_chain, 537_600 - 105);
        let mut client = client_with_node_account(on_chain.clone()).await;
        client.add_account(&cached, false).await.unwrap();
        assert_eq!(
            fee_balance(
                &client.get_account(cached.id()).await.unwrap().unwrap(),
                snapshot().fee_faucet_id
            ),
            53_319,
        );

        let balance = poll_account(&mut client, "bridge", cached.id(), &snapshot())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(balance, 590_814);
        assert_eq!(txns_left(balance, max_fee_per_txn(7)), 2_813);
        assert_eq!(
            client.get_account(cached.id()).await.unwrap(),
            Some(on_chain)
        );
    }

    #[tokio::test]
    async fn wallet_keeps_pending_local_state_ahead_of_node() {
        let on_chain = Account::builder([9; 32])
            .with_components(Auth::IncrNonce)
            .with_component(BasicWallet)
            .build_existing()
            .unwrap();
        let mut pending = on_chain.clone();
        credit(&mut pending, 1_000);
        let mut client = client_with_node_account(on_chain).await;
        client.add_account(&pending, false).await.unwrap();

        assert_eq!(
            poll_account(&mut client, "service", pending.id(), &snapshot())
                .await
                .unwrap(),
            Some(1_000),
        );
        assert_eq!(
            client.get_account(pending.id()).await.unwrap(),
            Some(pending)
        );
    }

    #[tokio::test]
    async fn failed_network_refresh_does_not_return_a_cached_balance() {
        let mut on_chain = network_account();
        credit(&mut on_chain, 1_000);
        let mut cached = on_chain.clone();
        credit(&mut cached, 500);
        let mut client = client_with_node_account(on_chain).await;
        client.add_account(&cached, false).await.unwrap();

        // The client refuses an import from a node whose nonce is behind the
        // stored account. Surface that failure instead of sampling stale data.
        let error = poll_account(&mut client, "bridge", cached.id(), &snapshot())
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<miden_client::ClientError>(),
            Some(miden_client::ClientError::AccountNonceTooLow),
        ));
    }

    #[tokio::test]
    async fn undeployed_and_untracked_accounts_need_no_node_refresh() {
        let mut client = crate::test_helpers::offline_miden_client_lib().await;
        let account = network_account();
        assert_eq!(
            poll_account(&mut client, "bridge", account.id(), &snapshot())
                .await
                .unwrap(),
            None,
        );
        client.add_account(&account, false).await.unwrap();
        assert_eq!(
            poll_account(&mut client, "bridge", account.id(), &snapshot())
                .await
                .unwrap(),
            Some(0),
        );
        assert!(
            client
                .get_account(account.id())
                .await
                .unwrap()
                .unwrap()
                .is_new()
        );
    }

    #[test]
    fn txns_left_is_conservative_and_zero_fee_safe() {
        assert_eq!(txns_left(13_440, 210), 64);
        assert_eq!(txns_left(209, 210), 0);
        assert_eq!(txns_left(5, 0), u64::MAX);
    }
}
