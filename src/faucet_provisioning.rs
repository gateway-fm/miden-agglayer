//! Durable identity and transaction handoffs for runtime faucet provisioning.
//!
//! A registry row describes a completed registration. It cannot also act as
//! the intent for funding/deployment: a restart before registration used to
//! generate another account and spend the same cascade budget twice.

use crate::miden_client::MidenClientLib;
use crate::store::Store;
use anyhow::{Context, ensure};
use miden_base_agglayer::{ConfigAggBridgeNote, ConversionMetadata, MetadataHash};
use miden_client::crypto::FeltRng;
use miden_client::note::NoteFile;
use miden_client::store::TransactionFilter;
use miden_client::transaction::{
    PaymentNoteDescription, TransactionRequest, TransactionRequestBuilder, TransactionResult,
};
use miden_client::{ClientError, Deserializable, Serializable};
use miden_protocol::Felt;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteType;
use miden_protocol::transaction::ProvenTransaction;
use miden_standards::interop::eth::EthAddress;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetDeployment {
    /// Bridge ID + origin network + token address; never a token symbol.
    pub key: String,
    /// Versioned immutable account/route parameters, including metadata hash.
    pub binding: Vec<u8>,
    /// Initial public network account, including its creation seed.
    pub initial_account: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaucetStep {
    Fund,
    Deploy,
    Register,
}

impl FaucetStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fund => "fund",
            Self::Deploy => "deploy",
            Self::Register => "register",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetPreparedTx {
    pub generation: u32,
    pub expiration_block: u64,
    /// Serialized TransactionResult, needed to replay and apply the exact tx.
    pub executed: Vec<u8>,
    /// Serialized ProvenTransaction. A retry does not execute/sign a new send.
    pub proven: Vec<u8>,
}

/// Shared store contract. Fresh absence is checked by the caller; the store
/// also prevents skipping generations or replacing a still-valid handoff.
pub(crate) fn check_next_generation(
    current: Option<&FaucetPreparedTx>,
    next: &FaucetPreparedTx,
    observed_height: u64,
) -> anyhow::Result<()> {
    match current {
        None => ensure!(
            next.generation == 0,
            "first faucet handoff must be generation zero"
        ),
        Some(current) => {
            ensure!(
                next.generation
                    == current
                        .generation
                        .checked_add(1)
                        .context("faucet generation overflow")?,
                "faucet handoff generation changed; reconcile before preparing again"
            );
            ensure!(
                observed_height > current.expiration_block,
                "prior faucet handoff has not expired"
            );
        }
    }
    ensure!(
        next.expiration_block > observed_height,
        "new faucet handoff already expired"
    );
    ensure!(
        !next.executed.is_empty() && !next.proven.is_empty(),
        "empty faucet handoff"
    );
    Ok(())
}

/// Runtime claim/admin path. Bootstrap and standalone tools keep their own
/// lifecycle; this journal belongs to the proxy's Store, not to the SDK cache.
#[allow(clippy::too_many_arguments)]
pub async fn create_and_register_faucet(
    client: &mut MidenClientLib,
    store: &dyn Store,
    symbol: &str,
    miden_decimals: u8,
    origin_token_address: &[u8; 20],
    origin_network: u32,
    scale: u8,
    service_id: AccountId,
    bridge_id: AccountId,
    metadata_hash: MetadataHash,
    is_native: bool,
) -> anyhow::Result<Account> {
    crate::miden_client::ensure_writable(service_id)?;
    client.sync_state().await?;
    // A stale local bridge snapshot must not authorize a replacement account.
    client.import_account_by_id(bridge_id).await?;
    let bridge = client
        .get_account(bridge_id)
        .await?
        .context("bridge missing after refresh")?;
    ensure!(
        crate::metadata_recovery::find_registered_faucet_for_origin(
            bridge.storage(),
            origin_token_address,
            origin_network,
        )
        .is_none(),
        "origin already registered on chain; retry through existing-faucet recovery"
    );

    let fee = crate::fee_funding::fee_snapshot(client).await?;
    let genesis = client
        .get_block_header_by_num(BlockNumber::GENESIS)
        .await?
        .context("genesis unavailable for faucet deployment binding")?
        .0;
    let key = format!(
        "{}/{}/{}",
        bridge_id.to_hex(),
        origin_network,
        hex::encode(origin_token_address)
    );
    let binding = serde_json::to_vec(&serde_json::json!({
        "version": 1, "genesis": hex::encode(genesis.commitment().as_bytes()),
        "service": service_id.to_hex(), "bridge": bridge_id.to_hex(),
        "origin_network": origin_network, "origin_address": hex::encode(origin_token_address),
        "symbol": symbol, "decimals": miden_decimals, "scale": scale,
        "metadata_hash": hex::encode(metadata_hash.as_bytes()), "is_native": is_native,
        "fee_faucet": fee.fee_faucet_id.to_hex(), "base_fee": fee.verification_base_fee,
        "cascade_amount": crate::fee_funding::cascade_amount(&fee),
    }))?;
    let deployment = if let Some(saved) = store.get_faucet_deployment(&key).await? {
        ensure!(
            saved.binding == binding,
            "faucet deployment binding mismatch"
        );
        saved
    } else {
        let account = crate::network_accounts::faucet_account_builder(
            client.rng().draw_word(),
            symbol,
            miden_decimals,
            Felt::new(u64::from(FungibleAsset::MAX_AMOUNT))?,
            Felt::new(0)?,
            service_id,
            bridge_id,
            fee.fee_faucet_id,
        )?
        .build()?;
        store
            .reserve_faucet_deployment(FaucetDeployment {
                key,
                binding,
                initial_account: account.to_bytes(),
            })
            .await?
    };
    let account = Account::read_from_bytes(&deployment.initial_account)?;
    if client.get_account(account.id()).await?.is_none() {
        client.add_account(&account, false).await?;
    }
    tracing::info!(faucet = %account.id(), origin_network,
        "resuming durable faucet provisioning");

    if fee.charges_fees() {
        drive_step(
            client,
            store,
            &deployment.key,
            FaucetStep::Fund,
            account.id(),
            service_id,
            async |client: &mut MidenClientLib| {
                let asset = Asset::Fungible(FungibleAsset::new(
                    fee.fee_faucet_id,
                    crate::fee_funding::cascade_amount(&fee),
                )?);
                Ok(TransactionRequestBuilder::new()
                    .expiration_delta(crate::claim::submission_note_expiration_delta())
                    .build_pay_to_id(
                        PaymentNoteDescription::new(vec![asset], service_id, account.id()),
                        NoteType::Public,
                        client.rng(),
                    )?)
            },
        )
        .await?;
    }

    // Construct a consume request only after reconciling any saved deploy.
    // A pending deploy replays its exact bytes; an expired, absent deploy can
    // consume the same funding note in a new transaction for the same account.
    drive_step(
        client,
        store,
        &deployment.key,
        FaucetStep::Deploy,
        account.id(),
        account.id(),
        async |client: &mut MidenClientLib| {
            let builder = TransactionRequestBuilder::new()
                .expiration_delta(crate::claim::submission_note_expiration_delta());
            if fee.charges_fees() {
                let notes = crate::fee_funding::wait_for_funding_notes(
                    client,
                    account.id(),
                    symbol,
                    &fee,
                    Duration::from_secs(120),
                )
                .await?;
                Ok(builder.build_consume_notes(notes)?)
            } else {
                Ok(builder.build()?)
            }
        },
    )
    .await?;

    drive_step(
        client,
        store,
        &deployment.key,
        FaucetStep::Register,
        account.id(),
        service_id,
        async |client: &mut MidenClientLib| {
            let note = ConfigAggBridgeNote::create(
                ConversionMetadata {
                    faucet_account_id: account.id(),
                    origin_token_address: EthAddress::new(*origin_token_address),
                    scale,
                    origin_network,
                    is_native,
                    metadata_hash,
                },
                service_id,
                bridge_id,
                client.rng(),
            )?;
            Ok(TransactionRequestBuilder::new()
                .own_output_notes(vec![note])
                .foreign_accounts([crate::miden_client::network_target_foreign_account(
                    bridge_id,
                )?])
                .expiration_delta(crate::claim::submission_note_expiration_delta())
                .build()?)
        },
    )
    .await?;
    Ok(account)
}

async fn deployed_account(client: &mut MidenClientLib, id: AccountId) -> anyhow::Result<bool> {
    match client.import_account_by_id(id).await {
        Ok(()) => Ok(!client
            .get_account(id)
            .await?
            .context("deployed faucet missing after import")?
            .is_new()),
        Err(ClientError::AccountNotFoundOnChain(missing)) if missing == id => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Query the exact public output note on the node, independently of whether
/// apply_transaction completed before the restart. A local Pending record or
/// an RPC error is never evidence of absence.
async fn step_effect(
    client: &mut MidenClientLib,
    step: FaucetStep,
    faucet: AccountId,
    result: &TransactionResult,
) -> anyhow::Result<bool> {
    if step == FaucetStep::Deploy {
        return deployed_account(client, faucet).await;
    }
    ensure!(
        result.created_notes().num_notes() == 1,
        "faucet handoff must contain exactly one output note"
    );
    let id = result
        .created_notes()
        .iter()
        .next()
        .context("missing faucet output note")?
        .id();
    match client.import_notes(&[NoteFile::NoteId(id)]).await {
        Ok(_) => Ok(true),
        Err(ClientError::NoteNotFoundOnChain(missing)) if missing == id => Ok(false),
        // Stock miden-client 0.16.0 import_notes maps a successful empty
        // GetNotesById response to this variant (the typed NoteNotFound is
        // used by get_note_by_id). This single-ID request makes empty an
        // exact absence. All RPC, conversion, and other import errors fail.
        Err(ClientError::NoteImportError(message)) if message == "No notes fetched from node" => {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn decode_handoff(
    saved: &FaucetPreparedTx,
    signer: AccountId,
) -> anyhow::Result<(TransactionResult, ProvenTransaction)> {
    let result = TransactionResult::read_from_bytes(&saved.executed)?;
    let proven = ProvenTransaction::read_from_bytes(&saved.proven)?;
    ensure!(
        result.id() == proven.id(),
        "faucet handoff transaction identity mismatch"
    );
    ensure!(
        result.executed_transaction().account_id() == signer && proven.account_id() == signer,
        "faucet handoff signer mismatch"
    );
    ensure!(
        proven.expiration_block_num().as_u64() == saved.expiration_block,
        "faucet handoff expiration mismatch"
    );
    Ok((result, proven))
}

#[allow(clippy::too_many_arguments)]
async fn drive_step<F>(
    client: &mut MidenClientLib,
    store: &dyn Store,
    key: &str,
    step: FaucetStep,
    faucet: AccountId,
    signer: AccountId,
    build_request: F,
) -> anyhow::Result<()>
where
    F: AsyncFnOnce(&mut MidenClientLib) -> anyhow::Result<TransactionRequest>,
{
    crate::miden_client::ensure_writable(signer)?;
    resume_step(
        store,
        key,
        step,
        &mut MidenStep {
            client,
            step,
            faucet,
            signer,
            build_request: Some(build_request),
        },
    )
    .await
}

// Keep the durable orchestration independent of the transport so restart and
// lost-response regressions exercise the same state machine as live sends.
trait StepRuntime {
    async fn sync_height(&mut self) -> anyhow::Result<u64>;
    async fn effect(&mut self, saved: &FaucetPreparedTx) -> anyhow::Result<bool>;
    async fn prepare(&mut self, generation: u32) -> anyhow::Result<FaucetPreparedTx>;
    async fn submit(&mut self, saved: &FaucetPreparedTx) -> anyhow::Result<()>;
}

async fn resume_step(
    store: &dyn Store,
    key: &str,
    step: FaucetStep,
    runtime: &mut impl StepRuntime,
) -> anyhow::Result<()> {
    let observed_height = runtime.sync_height().await?;
    let previous = store.get_faucet_step(key, step).await?;
    let mut generation = 0;
    if let Some(saved) = previous {
        if runtime.effect(&saved).await? {
            return Ok(());
        }
        if observed_height <= saved.expiration_block {
            return runtime.submit(&saved).await;
        }
        // Exact absence is checked AFTER syncing beyond the creating tx's
        // expiry. An unavailable node fails above; it cannot authorize a send.
        generation = saved
            .generation
            .checked_add(1)
            .context("faucet generation overflow")?;
    }
    let proposal = runtime.prepare(generation).await?;
    let saved = store
        .prepare_faucet_step(key, step, proposal, observed_height)
        .await?;
    // Another writer may have won. Only the durable winner may be submitted.
    runtime.submit(&saved).await
}

struct MidenStep<'a, F> {
    client: &'a mut MidenClientLib,
    step: FaucetStep,
    faucet: AccountId,
    signer: AccountId,
    build_request: Option<F>,
}

impl<F> StepRuntime for MidenStep<'_, F>
where
    F: AsyncFnOnce(&mut MidenClientLib) -> anyhow::Result<TransactionRequest>,
{
    async fn sync_height(&mut self) -> anyhow::Result<u64> {
        self.client.sync_state().await?;
        Ok(self.client.get_sync_height().await?.as_u64())
    }

    async fn effect(&mut self, saved: &FaucetPreparedTx) -> anyhow::Result<bool> {
        let (result, _) = decode_handoff(saved, self.signer)?;
        step_effect(self.client, self.step, self.faucet, &result).await
    }

    async fn prepare(&mut self, generation: u32) -> anyhow::Result<FaucetPreparedTx> {
        let build = self
            .build_request
            .take()
            .context("faucet request builder already used")?;
        let request = build(self.client).await?;
        let result = self
            .client
            .execute_transaction(self.signer, request)
            .await?;
        let proven = self.client.prove_transaction(&result).await?;
        Ok(FaucetPreparedTx {
            generation,
            expiration_block: proven.expiration_block_num().as_u64(),
            executed: result.to_bytes(),
            proven: proven.to_bytes(),
        })
    }

    async fn submit(&mut self, saved: &FaucetPreparedTx) -> anyhow::Result<()> {
        let (result, proven) = decode_handoff(saved, self.signer)?;
        tracing::info!(faucet = %self.faucet, step = self.step.as_str(), tx = %result.id(),
            "submitting durable faucet transaction");
        let height = self
            .client
            .submit_proven_transaction(proven, &result)
            .await?;
        if self
            .client
            .get_transactions(TransactionFilter::Ids(vec![result.id()]))
            .await?
            .is_empty()
        {
            self.client.apply_transaction(&result, height).await?;
        }
        let committed = crate::miden_client::wait_for_transaction_commit(
            self.client,
            result.id(),
            30,
            Duration::from_secs(2),
        )
        .await?;
        // A lost SDK apply may hide the transaction record. Exact node evidence
        // still permits progress; neither a timeout nor a send ACK does.
        ensure!(
            committed || step_effect(self.client, self.step, self.faucet, &result).await?,
            "faucet {} transaction {} not observed committed; durable handoff retained",
            self.step.as_str(),
            result.id(),
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "faucet_provisioning_tests.rs"]
pub(crate) mod tests;
