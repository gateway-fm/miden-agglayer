//! Initialization identity and prepared-send checkpoint. The lock spans the
//! entire init attempt; a second process cannot overwrite a pending identity.
use anyhow::{Context, ensure};
use miden_client::{Deserializable, Serializable};
use miden_protocol::account::{Account, AccountType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::faucet_provisioning::{
    FaucetPreparedTx, FaucetStep, StepJournal, check_next_generation,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InitBinding {
    pub genesis: String,
    pub network: u32,
    pub fee_faucet: String,
    pub base_fee: u32,
    pub cascade_amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    binding: InitBinding,
    /// Hex-encoded public accounts preserve the initial seed. They contain no
    /// signing secrets; those remain in the existing keystore.
    accounts: BTreeMap<String, String>,
    steps: BTreeMap<String, FaucetPreparedTx>,
}

pub(crate) struct InitCheckpoint {
    path: PathBuf,
    state: Mutex<Checkpoint>,
    // File locks are released by the OS even after a killed process.
    _lock: File,
}

impl InitCheckpoint {
    pub(crate) fn open(dir: Option<PathBuf>, binding: InitBinding) -> anyhow::Result<Self> {
        let path = super::config_path(dir)?.with_file_name("initialization.toml");
        fs::create_dir_all(
            path.parent()
                .context("initialization checkpoint has no directory")?,
        )?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.try_lock()
            .context("another initialization is active for this store directory")?;
        let state = if path.exists() {
            let state: Checkpoint = toml::from_str(&fs::read_to_string(&path)?).context(
                "invalid initialization checkpoint; refusing to replace saved identities",
            )?;
            ensure!(
                state.version == 1,
                "unsupported initialization checkpoint version"
            );
            ensure!(
                state.binding == binding,
                "initialization checkpoint chain/network/fee binding mismatch"
            );
            state
        } else {
            Checkpoint {
                version: 1,
                binding,
                accounts: BTreeMap::new(),
                steps: BTreeMap::new(),
            }
        };
        let checkpoint = Self {
            path,
            state: Mutex::new(state),
            _lock: lock,
        };
        checkpoint.persist(&checkpoint.state.lock().expect("checkpoint lock"))?;
        Ok(checkpoint)
    }

    fn persist(&self, state: &Checkpoint) -> anyhow::Result<()> {
        super::write_config_atomic(&self.path, &toml::to_string(state)?)
    }

    pub(crate) fn account(&self, name: &str) -> anyhow::Result<Option<Account>> {
        self.state
            .lock()
            .expect("checkpoint lock")
            .accounts
            .get(name)
            .map(|encoded| Ok(Account::read_from_bytes(&hex::decode(encoded)?)?))
            .transpose()
    }

    /// Never replace an identity, even before the first deployment. Service
    /// and GER identities therefore also bind every later prepared handoff.
    pub(crate) fn reserve_account(
        &self,
        name: &str,
        proposed: &Account,
    ) -> anyhow::Result<Account> {
        ensure!(
            proposed.id().account_type() != AccountType::Private,
            "initialization requires recoverable public accounts"
        );
        let mut state = self.state.lock().expect("checkpoint lock");
        if let Some(saved) = state.accounts.get(name) {
            let saved = Account::read_from_bytes(&hex::decode(saved)?)?;
            ensure!(
                saved.id() == proposed.id(),
                "initialization {name} identity mismatch"
            );
            return Ok(saved);
        }
        let mut next = state.clone();
        next.accounts
            .insert(name.to_owned(), hex::encode(proposed.to_bytes()));
        self.persist(&next)?;
        *state = next;
        Ok(proposed.clone())
    }

    fn step_key(key: &str, step: FaucetStep) -> String {
        format!("{key}/{}", step.as_str())
    }
}

impl StepJournal for InitCheckpoint {
    fn proof_kind(&self, key: &str, step: FaucetStep) -> Option<crate::metrics::ProofKind> {
        // fund_from_service historically attributed every cascade to Init;
        // faucet deployment/registration were attributed to Faucet.
        Some(if key == "faucet_eth" && step != FaucetStep::Fund {
            crate::metrics::ProofKind::Faucet
        } else {
            crate::metrics::ProofKind::Init
        })
    }

    async fn load(&self, key: &str, step: FaucetStep) -> anyhow::Result<Option<FaucetPreparedTx>> {
        Ok(self
            .state
            .lock()
            .expect("checkpoint lock")
            .steps
            .get(&Self::step_key(key, step))
            .cloned())
    }

    async fn prepare(
        &self,
        key: &str,
        step: FaucetStep,
        proposal: FaucetPreparedTx,
        observed_height: u64,
    ) -> anyhow::Result<FaucetPreparedTx> {
        let mut state = self.state.lock().expect("checkpoint lock");
        ensure!(
            state.accounts.contains_key(key),
            "initialization handoff has no saved account"
        );
        let slot = Self::step_key(key, step);
        if let Some(saved) = state.steps.get(&slot)
            && saved.generation == proposal.generation
        {
            return Ok(saved.clone());
        }
        check_next_generation(state.steps.get(&slot), &proposal, observed_height)?;
        let mut next = state.clone();
        next.steps.insert(slot, proposal.clone());
        self.persist(&next)?;
        *state = next;
        Ok(proposal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_client::testing::Auth;
    use miden_standards::account::wallets::BasicWallet;

    pub(crate) fn binding() -> InitBinding {
        InitBinding {
            genesis: "genesis-A".into(),
            network: 7,
            fee_faucet: "fee-A".into(),
            base_fee: 0,
            cascade_amount: 0,
        }
    }
    pub(crate) fn account(seed: u8) -> Account {
        Account::builder([seed; 32])
            .account_type(AccountType::Public)
            .with_components(Auth::IncrNonce)
            .with_component(BasicWallet)
            .build()
            .unwrap()
    }

    #[test]
    fn initial_identities_survive_restart_and_reject_chain_or_role_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = Some(dir.path().to_path_buf());
        let checkpoint = InitCheckpoint::open(path.clone(), binding()).unwrap();
        let service = account(61);
        let ger = account(62);
        checkpoint.reserve_account("service", &service).unwrap();
        checkpoint.reserve_account("ger_manager", &ger).unwrap();
        assert!(
            InitCheckpoint::open(path.clone(), binding()).is_err(),
            "second initializer acquired the lock"
        );
        drop(checkpoint);
        let restarted = InitCheckpoint::open(path.clone(), binding()).unwrap();
        assert_eq!(restarted.account("service").unwrap().unwrap(), service);
        assert_eq!(restarted.account("ger_manager").unwrap().unwrap(), ger);
        assert!(restarted.reserve_account("service", &ger).is_err());
        assert_eq!(
            restarted.account("service").unwrap().unwrap().seed(),
            service.seed()
        );
        drop(restarted);
        for field in 0..4 {
            let mut changed = binding();
            match field {
                0 => changed.genesis.push('B'),
                1 => changed.network += 1,
                2 => changed.fee_faucet.push('B'),
                _ => changed.base_fee = 7,
            }
            assert!(InitCheckpoint::open(path.clone(), changed).is_err());
        }
    }

    #[test]
    fn bootstrap_preserves_proof_attribution_without_metering_runtime_replays() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = InitCheckpoint::open(Some(dir.path().to_path_buf()), binding()).unwrap();
        for key in ["service", "ger_manager", "bridge", "faucet_eth"] {
            for step in [FaucetStep::Fund, FaucetStep::Deploy, FaucetStep::Register] {
                let expected = if key == "faucet_eth" && step != FaucetStep::Fund {
                    "faucet"
                } else {
                    "init"
                };
                assert_eq!(
                    checkpoint.proof_kind(key, step).unwrap().as_label(),
                    expected
                );
            }
        }
        let runtime = crate::store::memory::InMemoryStore::new();
        assert!(runtime.proof_kind("runtime", FaucetStep::Deploy).is_none());
    }

    #[test]
    fn failed_persistence_never_exposes_an_unsaved_identity() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = InitCheckpoint::open(Some(dir.path().to_path_buf()), binding()).unwrap();
        let proposed = account(63);
        // Force the atomic writer to fail before rename, without changing the
        // already durable checkpoint or relying on filesystem permissions.
        let obstruction = dir.path().join("initialization.toml.new");
        fs::create_dir(&obstruction).unwrap();
        assert!(checkpoint.reserve_account("bridge", &proposed).is_err());
        assert!(checkpoint.account("bridge").unwrap().is_none());
        fs::remove_dir(obstruction).unwrap();
        checkpoint.reserve_account("bridge", &proposed).unwrap();
        drop(checkpoint);
        let reopened = InitCheckpoint::open(Some(dir.path().to_path_buf()), binding()).unwrap();
        assert_eq!(reopened.account("bridge").unwrap().unwrap(), proposed);
    }

    #[test]
    fn corrupt_checkpoint_never_generates_replacement_identities() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("initialization.toml"), "truncated = [").unwrap();
        assert!(InitCheckpoint::open(Some(dir.path().to_path_buf()), binding()).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("initialization.toml")).unwrap(),
            "truncated = ["
        );
    }
}
