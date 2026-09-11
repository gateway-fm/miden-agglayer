//! Fee-vault monitor (#201): exports the native fee-asset balance of every
//! account the proxy depends on, so an operator can alert before one hits 0.
//!
//! On a fee-charging chain every account pays its own transaction fees from
//! its vault — `service` on every claim, `ger_manager` on every GER injection,
//! the bridge on every network transaction (UpdateGer, CLAIM, B2AGG…), each
//! faucet on every MINT/BURN. An empty vault silently stalls that account
//! (its transactions abort in the kernel). Measured at base fee 7: network
//! transactions cost ~40–50 units, signed client transactions the 210 cap; a
//! busy bridge ran 27 network transactions in ten minutes — so the cascade
//! amounts are hours, not weeks, of runway, and the balance has to be watched.
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
//! ERROR at 0. Read-only; the proxy's tracked client state is what is read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use miden_protocol::account::{Account, AccountId};
use miden_protocol::asset::Asset;
use tokio::sync::oneshot;

use crate::fee_funding::{fee_snapshot, max_fee_per_txn};
use crate::miden_client::MidenClient;
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
                        let Some(account) = client.get_account(id).await? else {
                            tracing::warn!(account = %name, id = %id.to_hex(), "fee-vault monitor: account not in the client store");
                            continue;
                        };
                        // Sweep top-ups: a P2ID of the fee asset sent to this account
                        // lands only once consumed. Not for an account still being
                        // created — its cascade note is the deploy's to consume.
                        match if account.is_new() {
                            Ok(0)
                        } else {
                            crate::fee_funding::consume_fee_notes(client, id, &snap).await
                        } {
                            Ok(n) if n > 0 => tracing::info!(account = %name, notes = n, "consuming fee-asset top-up note(s) (#201)"),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(account = %name, error = %format!("{e:#}"), "could not consume a fee-asset top-up"),
                        }
                        let balance = fee_balance(&account, snap.fee_faucet_id);
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
                        if balance == 0 {
                            tracing::error!(account = %name, id = %id.to_hex(),
                                "fee vault EMPTY — every transaction of this account will abort until it is topped up (#201)");
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

    #[test]
    fn txns_left_is_conservative_and_zero_fee_safe() {
        assert_eq!(txns_left(13_440, 210), 64);
        assert_eq!(txns_left(209, 210), 0);
        assert_eq!(txns_left(5, 0), u64::MAX);
    }
}
