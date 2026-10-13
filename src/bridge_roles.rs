//! One-shot bridge RBAC role management (`--grant-bridge-role` /
//! `--revoke-bridge-role`).
//!
//! The bridge account's privileged roles (`ADMIN`, `FAUCET_MNGR`,
//! `GER_INJECTOR`, `GER_REMOVER`, `FEE_MNGR`, `PAUSER`) live in its
//! `RoleBasedAccessControl` component. Membership can only change on-chain,
//! through an [`RbacConfigNote`] the bridge consumes (via the ntx-builder), and
//! the RBAC procedures authorize against the NOTE SENDER — so the note must be
//! created and submitted by an account that currently holds the role's admin
//! role. At creation that is only `service` (`init::add_bridge`), whose key is
//! in the KMS behind the remote signer. This module is therefore run by the
//! proxy binary itself, with the production store and custody flags, as a
//! one-shot like `--print-funding` (see `docs/operations/bridge-admin-rbac.md`).
//!
//! Safety rails:
//! - roles are parsed from the fixed bridge set, never free-form;
//! - a revoke that would strip `service` of any role is refused: the proxy signs
//!   every faucet registration, GER injection and fee change as `service`;
//! - a revoke that would leave a role with no member is refused (a memberless
//!   `ADMIN` can never be re-populated);
//! - the bridge's note allowlist is checked for the `RbacConfigNote` root before
//!   anything is submitted — a bridge created by pre-0.17 code lacks it and
//!   would never consume the note;
//! - the sender's own membership of the role's admin role is checked up front,
//!   because a note that fails that check on-chain stays pending forever and
//!   revives the moment the sender regains the role (upstream docs).
//! - the change is confirmed by reading the bridge's membership map back from
//!   the node, not by the `service` transaction committing (that only proves
//!   the note exists).

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use miden_base_agglayer::AggLayerBridge;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::{Account, AccountId, RoleSymbol, StorageMapKey};
use miden_protocol::note::{Note, NoteId};
use miden_protocol::transaction::TransactionId;
use miden_protocol::{Felt, Word};
use miden_standards::account::access::RoleBasedAccessControl;
use miden_standards::account::auth::NetworkAccount;
use miden_standards::note::config::{RbacConfig, RbacConfigNote};

use crate::accounts_config::AccountIdBech32;
use crate::miden_client::MidenClientLib;

/// The bridge's role symbols, exactly as `AggLayerBridge` / the RBAC component
/// define them. Only these are accepted on the command line.
pub const BRIDGE_ROLES: [&str; 6] = [
    "ADMIN",
    "FAUCET_MNGR",
    "GER_INJECTOR",
    "GER_REMOVER",
    "FEE_MNGR",
    "PAUSER",
];

/// Resolves a bridge role by name (case-insensitive) to the symbol the bridge
/// account stores. Unknown names are an error, never a new role.
pub fn bridge_role(name: &str) -> anyhow::Result<RoleSymbol> {
    let upper = name.trim().to_ascii_uppercase();
    Ok(match upper.as_str() {
        "ADMIN" => RoleBasedAccessControl::admin_role(),
        "FAUCET_MNGR" => AggLayerBridge::faucet_manager_role(),
        "GER_INJECTOR" => AggLayerBridge::ger_injector_role(),
        "GER_REMOVER" => AggLayerBridge::ger_remover_role(),
        "FEE_MNGR" => AggLayerBridge::fee_manager_role(),
        "PAUSER" => AggLayerBridge::pauser_role(),
        other => bail!(
            "unknown bridge role {other:?}; expected one of {}",
            BRIDGE_ROLES.join(", ")
        ),
    })
}

/// Parses an account id in either 0x-hex or bech32 form.
pub fn parse_account_id(s: &str) -> anyhow::Result<AccountId> {
    let s = s.trim();
    if let Ok(id) = AccountId::from_hex(s) {
        return Ok(id);
    }
    if let Ok((_, id)) = AccountId::from_bech32(s) {
        return Ok(id);
    }
    Err(anyhow!(
        "cannot parse account id {s:?} (expected 0x-hex or bech32)"
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleAction {
    Grant,
    Revoke,
}

impl RoleAction {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleAction::Grant => "grant",
            RoleAction::Revoke => "revoke",
        }
    }
}

/// One requested membership change on the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeRoleChange {
    pub action: RoleAction,
    pub role: RoleSymbol,
    pub account: AccountId,
}

impl BridgeRoleChange {
    /// Parses the `<ROLE>=<ACCOUNT>` command-line spelling.
    pub fn parse(action: RoleAction, spec: &str) -> anyhow::Result<Self> {
        let (role, account) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("expected <ROLE>=<ACCOUNT> (e.g. ADMIN=0x…), got {spec:?}"))?;
        Ok(Self {
            action,
            role: bridge_role(role)?,
            account: parse_account_id(account)?,
        })
    }

    fn config(&self) -> RbacConfig {
        match self.action {
            RoleAction::Grant => RbacConfig::GrantRole {
                role: self.role.clone(),
                account: self.account,
            },
            RoleAction::Revoke => RbacConfig::RevokeRole {
                role: self.role.clone(),
                account: self.account,
            },
        }
    }
}

// STORAGE READERS
// ================================================================================================

/// Membership map key, exactly as the RBAC component writes it:
/// `[0, role_symbol, account_suffix, account_prefix]`.
pub fn membership_key(role: &RoleSymbol, account: AccountId) -> StorageMapKey {
    StorageMapKey::new(Word::from([
        Felt::ZERO,
        role.as_element(),
        account.suffix(),
        account.prefix().as_felt(),
    ]))
}

/// Role-config map key: `[0, 0, 0, role_symbol]`.
pub fn role_config_key(role: &RoleSymbol) -> StorageMapKey {
    StorageMapKey::new(Word::from([
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        role.as_element(),
    ]))
}

const MEMBER: Word = Word::new([Felt::ONE, Felt::ZERO, Felt::ZERO, Felt::ZERO]);

/// Whether `member` holds `role` on this (bridge) account.
pub fn holds_role(account: &Account, role: &RoleSymbol, member: AccountId) -> anyhow::Result<bool> {
    let value = account
        .storage()
        .get_map_item(
            RoleBasedAccessControl::role_membership_slot(),
            membership_key(role, member),
        )
        .map_err(|e| anyhow!("reading the RBAC membership map: {e}"))?;
    Ok(value == MEMBER)
}

/// Number of accounts holding `role` (0 = the role does not exist).
pub fn member_count(account: &Account, role: &RoleSymbol) -> anyhow::Result<u64> {
    let value = account
        .storage()
        .get_map_item(
            RoleBasedAccessControl::role_config_slot(),
            role_config_key(role),
        )
        .map_err(|e| anyhow!("reading the RBAC role-config map: {e}"))?;
    Ok(value.as_elements()[0].as_canonical_u64())
}

/// The role that administers `role`: its delegated admin if one is set, else
/// the built-in `ADMIN` role (the MASM falls back to `ADMIN` for a memberless
/// delegate too, which is why the memberless case is also mapped here).
pub fn effective_admin_role(account: &Account, role: &RoleSymbol) -> anyhow::Result<RoleSymbol> {
    let value = account
        .storage()
        .get_map_item(
            RoleBasedAccessControl::role_config_slot(),
            role_config_key(role),
        )
        .map_err(|e| anyhow!("reading the RBAC role-config map: {e}"))?;
    let delegated = value.as_elements()[1];
    if delegated == Felt::ZERO {
        return Ok(RoleBasedAccessControl::admin_role());
    }
    let delegated = RoleSymbol::try_from(delegated)
        .map_err(|e| anyhow!("role {role} has an invalid delegated admin symbol: {e}"))?;
    if member_count(account, &delegated)? == 0 {
        return Ok(RoleBasedAccessControl::admin_role());
    }
    Ok(delegated)
}

/// Whether the bridge's network-account note allowlist contains the
/// `RbacConfigNote` script root (true for bridges created by ≥0.17 code).
pub fn allows_rbac_config_note(account: &Account) -> anyhow::Result<bool> {
    let network = NetworkAccount::new(account.clone())
        .map_err(|e| anyhow!("the bridge is not a valid network account: {e}"))?;
    Ok(network
        .allowed_notes()
        .allowed_script_roots()
        .contains(&RbacConfigNote::script_root()))
}

async fn fetch_bridge(
    client: &mut MidenClientLib,
    bridge_id: AccountId,
) -> anyhow::Result<Account> {
    // Force-refresh from the node: the tracked copy of a foreign account can
    // lag (see `heal_bridge_after_executor_failure`).
    client
        .import_account_by_id(bridge_id)
        .await
        .map_err(|e| anyhow!("refreshing bridge account {bridge_id} from the node: {e}"))?;
    client
        .get_account(bridge_id)
        .await
        .map_err(|e| anyhow!("get_account({bridge_id}): {e}"))?
        .ok_or_else(|| anyhow!("bridge account {bridge_id} is not available after import"))
}

// EXECUTION
// ================================================================================================

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleChangeOutcome {
    /// The bridge already had the requested membership; nothing was submitted.
    AlreadyApplied,
    /// The note was submitted; `confirmed` is whether the bridge's membership
    /// map showed the change before the wait budget ran out.
    Submitted {
        tx_id: TransactionId,
        note_id: NoteId,
        confirmed: bool,
    },
}

/// Applies `change` to the bridge from `sender` (the configured `service`
/// account, the bridge's initial `ADMIN`).
///
/// `wait` bounds how long to poll the bridge for the membership change after
/// the note-creating transaction commits; zero skips the wait.
pub async fn apply_bridge_role_change(
    client: &mut MidenClientLib,
    service_id: AccountId,
    bridge_id: AccountId,
    change: &BridgeRoleChange,
    wait: Duration,
) -> anyhow::Result<RoleChangeOutcome> {
    if change.action == RoleAction::Revoke && change.account == service_id {
        bail!(
            "refusing to revoke {} from the service account {}: the proxy signs every bridge \
             operation as service. Rotate the service account through a designed migration \
             instead (docs/operations/provisioning.md, Rotation).",
            change.role,
            AccountIdBech32(service_id)
        );
    }

    client
        .sync_state()
        .await
        .map_err(|e| anyhow!("sync_state before the role change: {e}"))?;
    let bridge = fetch_bridge(client, bridge_id).await?;

    if !allows_rbac_config_note(&bridge)? {
        bail!(
            "bridge {} does not allowlist the RbacConfigNote script ({}); it was created by \
             pre-0.17 code. Add the root with a NetworkAccountConfigNote (AddAllowedNoteScript) \
             from service first — a separate, reviewed change.",
            AccountIdBech32(bridge_id),
            RbacConfigNote::script_root()
        );
    }

    let admin_role = effective_admin_role(&bridge, &change.role)?;
    if !holds_role(&bridge, &admin_role, service_id)? {
        bail!(
            "service account {} does not hold {admin_role}, which administers {}; the note would \
             never execute (and would stay pending). Nothing submitted.",
            AccountIdBech32(service_id),
            change.role
        );
    }

    let currently_member = holds_role(&bridge, &change.role, change.account)?;
    let count = member_count(&bridge, &change.role)?;
    match change.action {
        RoleAction::Grant if currently_member => {
            tracing::info!(
                role = %change.role,
                account = %AccountIdBech32(change.account),
                member_count = count,
                "account already holds the role; nothing to do"
            );
            return Ok(RoleChangeOutcome::AlreadyApplied);
        }
        RoleAction::Revoke if !currently_member => {
            bail!(
                "account {} does not hold {} (member_count={count}); nothing to revoke",
                AccountIdBech32(change.account),
                change.role
            );
        }
        RoleAction::Revoke if count <= 1 => {
            bail!(
                "refusing to revoke the last member of {}: a memberless role can never be \
                 re-populated",
                change.role
            );
        }
        _ => {}
    }

    let note: Note = RbacConfigNote::builder()
        .sender(service_id)
        .target(bridge_id)
        .config(change.config())
        .generate_serial_number(client.rng())
        .build()
        .map_err(|e| anyhow!("building the RbacConfigNote: {e}"))?
        .into();
    let note_id = note.id();

    let txn = TransactionRequestBuilder::new()
        .own_output_notes(vec![note])
        // The fee manager FPIs into the note's network target (the bridge),
        // exactly like a CONFIG note — declare it so the fetch is pinned.
        .foreign_accounts([crate::miden_client::network_target_foreign_account(
            bridge_id,
        )?])
        .build()
        .map_err(|e| anyhow!("building the role-change transaction: {e}"))?;

    tracing::info!(
        action = change.action.as_str(),
        role = %change.role,
        account = %AccountIdBech32(change.account),
        bridge = %AccountIdBech32(bridge_id),
        sender = %AccountIdBech32(service_id),
        note_id = %note_id,
        member_count_before = count,
        "submitting bridge role change"
    );

    let tx_id = match crate::miden_client::submit_new_transaction(client, service_id, txn).await {
        Ok(id) => id,
        Err(e) => {
            crate::miden_client::heal_bridge_after_executor_failure_anyhow(client, bridge_id, &e)
                .await;
            return Err(e.context("submitting the role-change transaction"));
        }
    };
    tracing::info!(tx_id = %tx_id, note_id = %note_id, "role-change transaction submitted");

    let committed =
        crate::miden_client::wait_for_transaction_commit(client, tx_id, 60, Duration::from_secs(2))
            .await
            .context("waiting for the role-change transaction to commit")?;
    if !committed {
        bail!(
            "role-change transaction {tx_id} (note {note_id}) was not observed committed. Do not \
             re-issue blindly: check the transaction on the node first — the note, if created, \
             is a standing instruction the bridge will still consume."
        );
    }

    let expected_member = change.action == RoleAction::Grant;
    let confirmed = wait_for_membership(
        client,
        bridge_id,
        &change.role,
        change.account,
        expected_member,
        wait,
    )
    .await?;

    if confirmed {
        let bridge = fetch_bridge(client, bridge_id).await?;
        tracing::info!(
            action = change.action.as_str(),
            role = %change.role,
            account = %AccountIdBech32(change.account),
            member_count_after = member_count(&bridge, &change.role)?,
            "bridge role change confirmed on-chain"
        );
    } else {
        tracing::warn!(
            note_id = %note_id,
            wait_secs = wait.as_secs(),
            "the bridge has not consumed the role-change note yet; it is pending on-chain. \
             Check the ntx-builder and the bridge's fee vault, then re-read the membership \
             (do NOT re-issue the note)."
        );
    }

    Ok(RoleChangeOutcome::Submitted {
        tx_id,
        note_id,
        confirmed,
    })
}

/// Polls the bridge's membership map until `member`'s membership of `role`
/// equals `expected` or `budget` elapses. Returns whether it was observed.
async fn wait_for_membership(
    client: &mut MidenClientLib,
    bridge_id: AccountId,
    role: &RoleSymbol,
    member: AccountId,
    expected: bool,
    budget: Duration,
) -> anyhow::Result<bool> {
    const POLL: Duration = Duration::from_secs(5);
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let bridge = fetch_bridge(client, bridge_id).await?;
        if holds_role(&bridge, role, member)? == expected {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(POLL.min(deadline - tokio::time::Instant::now())).await;
    }
}

/// Runs one role change through the client actor, after rebuilding and
/// verifying the remote custody bindings (the signer must prove it holds the
/// `service` key before anything is signed).
pub async fn run_one_shot(
    client: &crate::miden_client::MidenClient,
    accounts: crate::accounts_config::AccountsConfig,
    change: BridgeRoleChange,
    wait: Duration,
) -> anyhow::Result<RoleChangeOutcome> {
    let keystore = client.get_keystore();
    let outcome = std::sync::Arc::new(std::sync::OnceLock::<RoleChangeOutcome>::new());
    let outcome_internal = outcome.clone();
    client
        .with(move |client| {
            Box::new(async move {
                crate::init::verify_remote_bindings(client, &keystore, &accounts).await?;
                let result = apply_bridge_role_change(
                    client,
                    accounts.service.0,
                    accounts.bridge.0,
                    &change,
                    wait,
                )
                .await?;
                outcome_internal
                    .set(result)
                    .map_err(|_| anyhow!("role-change outcome already set"))?;
                Ok(())
            })
        })
        .await?;
    outcome
        .get()
        .cloned()
        .ok_or_else(|| anyhow!("role change produced no outcome"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_base_agglayer::BridgeRoles;
    use std::collections::BTreeSet;

    fn id(hex: &str) -> AccountId {
        AccountId::from_hex(hex).unwrap()
    }

    /// A bridge account exactly as `init::add_bridge` builds it: `service` is
    /// ADMIN + FAUCET_MNGR + FEE_MNGR + PAUSER, `ger_manager` holds both GER
    /// roles.
    fn test_bridge(service: AccountId, ger_manager: AccountId) -> Account {
        let fee_faucet = id("0x9a0000000000dd110000ee000000fc");
        let roles = BridgeRoles::new(
            BTreeSet::from([service]),
            BTreeSet::from([ger_manager]),
            BTreeSet::from([ger_manager]),
            BTreeSet::from([service]),
            BTreeSet::from([service]),
        )
        .unwrap();
        crate::network_accounts::bridge_account_builder([7u8; 32], service, roles, 1, fee_faucet)
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn roles_resolve_to_the_bridge_symbols_case_insensitively() {
        assert_eq!(
            bridge_role("ADMIN").unwrap(),
            RoleBasedAccessControl::admin_role()
        );
        assert_eq!(
            bridge_role("admin").unwrap(),
            RoleBasedAccessControl::admin_role()
        );
        assert_eq!(
            bridge_role(" faucet_mngr ").unwrap(),
            AggLayerBridge::faucet_manager_role()
        );
        assert_eq!(
            bridge_role("GER_INJECTOR").unwrap(),
            AggLayerBridge::ger_injector_role()
        );
        assert_eq!(
            bridge_role("GER_REMOVER").unwrap(),
            AggLayerBridge::ger_remover_role()
        );
        assert_eq!(
            bridge_role("FEE_MNGR").unwrap(),
            AggLayerBridge::fee_manager_role()
        );
        assert_eq!(
            bridge_role("PAUSER").unwrap(),
            AggLayerBridge::pauser_role()
        );
        for name in BRIDGE_ROLES {
            bridge_role(name).unwrap();
        }
        let err = bridge_role("OWNER").unwrap_err().to_string();
        assert!(err.contains("unknown bridge role"), "{err}");
    }

    #[test]
    fn parses_role_equals_account_and_rejects_malformed_specs() {
        let change =
            BridgeRoleChange::parse(RoleAction::Grant, "ADMIN=0x93a6e276fa374b417db5b93f716e0d")
                .unwrap();
        assert_eq!(change.action, RoleAction::Grant);
        assert_eq!(change.role, RoleBasedAccessControl::admin_role());
        assert_eq!(change.account, id("0x93a6e276fa374b417db5b93f716e0d"));

        let bech32 = change
            .account
            .to_bech32(miden_protocol::address::NetworkId::Testnet);
        let via_bech32 =
            BridgeRoleChange::parse(RoleAction::Revoke, &format!("pauser={bech32}")).unwrap();
        assert_eq!(via_bech32.account, change.account);
        assert_eq!(via_bech32.role, AggLayerBridge::pauser_role());

        assert!(BridgeRoleChange::parse(RoleAction::Grant, "ADMIN").is_err());
        assert!(BridgeRoleChange::parse(RoleAction::Grant, "ADMIN=0x1234").is_err());
        assert!(
            BridgeRoleChange::parse(RoleAction::Grant, "ROOT=0x93a6e276fa374b417db5b93f716e0d")
                .is_err()
        );
    }

    #[test]
    fn membership_readers_match_the_rbac_component_layout() {
        let service = id("0xac0000000000dd110000ee000000ad");
        let ger_manager = id("0xac0000000000dd110000ee000000fd");
        let stranger = id("0x93a6e276fa374b417db5b93f716e0d");
        let bridge = test_bridge(service, ger_manager);
        let admin = RoleBasedAccessControl::admin_role();

        assert!(holds_role(&bridge, &admin, service).unwrap());
        assert!(!holds_role(&bridge, &admin, ger_manager).unwrap());
        assert!(!holds_role(&bridge, &admin, stranger).unwrap());
        assert_eq!(member_count(&bridge, &admin).unwrap(), 1);

        let injector = AggLayerBridge::ger_injector_role();
        assert!(holds_role(&bridge, &injector, ger_manager).unwrap());
        assert!(!holds_role(&bridge, &injector, service).unwrap());
        assert_eq!(member_count(&bridge, &injector).unwrap(), 1);

        // Every bridge role is administered by ADMIN at creation.
        for name in BRIDGE_ROLES {
            assert_eq!(
                effective_admin_role(&bridge, &bridge_role(name).unwrap()).unwrap(),
                admin,
                "{name}"
            );
        }
        // An unknown role has no members and no config.
        let unknown = RoleSymbol::new("NOBODY").unwrap();
        assert_eq!(member_count(&bridge, &unknown).unwrap(), 0);
        assert!(!holds_role(&bridge, &unknown, service).unwrap());
    }

    /// The whole mechanism rests on the bridge consuming `RbacConfigNote`s:
    /// a freshly built bridge must allowlist the script, and the reader must
    /// see it.
    #[test]
    fn a_fresh_bridge_allowlists_the_rbac_config_note() {
        let bridge = test_bridge(
            id("0xac0000000000dd110000ee000000ad"),
            id("0xac0000000000dd110000ee000000fd"),
        );
        assert!(allows_rbac_config_note(&bridge).unwrap());
        assert!(AggLayerBridge::allowed_notes().contains(&RbacConfigNote::script_root()));
    }

    /// The note the one-shot submits is a public, bridge-targeted network
    /// note whose storage selects GrantRole for the requested account.
    #[test]
    fn grant_note_targets_the_bridge_and_encodes_the_grant() {
        use miden_standards::note::NetworkAccountTarget;
        let service = id("0xac0000000000dd110000ee000000ad");
        let bridge = id("0xac0000000000dd110000ee000000fc");
        let change =
            BridgeRoleChange::parse(RoleAction::Grant, "ADMIN=0x93a6e276fa374b417db5b93f716e0d")
                .unwrap();
        let rbac_note = RbacConfigNote::builder()
            .sender(service)
            .target(bridge)
            .config(change.config())
            .serial_number(Word::from([Felt::ONE; 4]))
            .build()
            .unwrap();
        assert_eq!(
            NetworkAccountTarget::try_from(rbac_note.attachments())
                .unwrap()
                .target_id(),
            bridge
        );
        assert_eq!(
            rbac_note.config(),
            &RbacConfig::GrantRole {
                role: RoleBasedAccessControl::admin_role(),
                account: change.account,
            }
        );
        let note: Note = rbac_note.into();
        assert_eq!(note.metadata().sender(), service);
        assert_eq!(
            note.metadata().note_type(),
            miden_protocol::note::NoteType::Public
        );
        assert_eq!(note.script().root(), RbacConfigNote::script_root());
    }
}
