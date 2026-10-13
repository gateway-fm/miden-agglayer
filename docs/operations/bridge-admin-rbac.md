# Changing bridge roles: granting a second `ADMIN` (KMS custody)

**Worked request:** make `0x93a6e276fa374b417db5b93f716e0d` an `ADMIN` of the
Miden bridge account, in addition to the current admin.

This is the procedure for the production custody model, where the account keys
live in a KMS behind Web3Signer and the proxy binary is the only thing that can
sign as the bridge's current admin. The change is made with the proxy's
one-shot flags `--grant-bridge-role` / `--revoke-bridge-role`
(`src/bridge_roles.rs`), run against the production store and custody flags
with the serving proxy stopped.

## 1. What `BRIDGE_ADMIN` is on-chain

The bridge is a Miden **network account** built in `src/network_accounts.rs`
(`bridge_account_builder`) from the upstream `AggLayerBridge` components plus
`RoleBasedAccessControl` (RBAC, from `miden-standards`). Roles and their initial
holders are fixed at creation in `src/init.rs::add_bridge`:

| Role | Initial holder | Gates |
|---|---|---|
| `ADMIN` (built-in) | `service` account | administers every role below and itself; may **unpause** the bridge |
| `FAUCET_MNGR` | `service` | `register_faucet`, `store_faucet_metadata_hash`, `deregister_faucet` (CONFIG / DEREGISTER notes) |
| `GER_INJECTOR` | `ger_manager` | `update_ger` |
| `GER_REMOVER` | `ger_manager` | `remove_ger` |
| `FEE_MNGR` | `service` | `set_note_fee` |
| `PAUSER` | `service` | `pause` (not unpause) |

"BRIDGE_ADMIN" in the request is the built-in `ADMIN` role. Facts that shape the
procedure:

- **Membership is a set, not a slot.** Granting a second member does not touch the
  first. "In addition to the current one" is the only safe reading: the proxy signs
  every faucet registration, GER injection and fee change as `service`, so
  `service` must keep `ADMIN` (and its other roles) or the bridge stops being
  operable by the proxy. The one-shot refuses to revoke anything from `service`.
- **Storage layout** (what the one-shot reads back to confirm). Two storage maps
  on the bridge account:
  - `role_config`: key `[0, 0, 0, role_symbol]` → `[member_count, admin_role_symbol, 0, 0]`
  - `role_membership`: key `[0, role_symbol, account_suffix, account_prefix]` → `[1, 0, 0, 0]`
- **A new `ADMIN` is as powerful as the KMS key.** It can grant and revoke every
  role, including revoking `service`, and it can unpause after an emergency pause.
  Treat the key that controls `0x93a6…` with the same change control as the
  Web3Signer keys. Record the grant in the deployment inventory.

## 2. The only path that changes membership: an `RbacConfigNote`

The RBAC procedures `grant_role` / `revoke_role` / `set_role_admin` /
`renounce_role` are account procedures that can only be invoked by `call` from a
**note script**, and they authorize against the **note sender**
(`active_note::get_sender`). There is no transaction script path and no
off-chain shortcut. The standard note for this is `RbacConfigNote`
(`miden_standards::note::config::RbacConfigNote`):

- storage `[0, role_symbol, account_suffix, account_prefix]` selects `GrantRole`;
- it is **public** and bound to the bridge by a `NetworkAccountTarget` attachment;
- it is consumed by the **bridge itself** through the node's ntx-builder, like a
  CONFIG or UPDATE_GER note. Nothing in the proxy has to "execute" it;
- the **sender must currently hold the role's admin role** (`ADMIN` for every
  bridge role at creation). Today that is only `service`.

Two preconditions follow, and the one-shot checks both before submitting:

1. **The bridge's note allowlist must contain the `RbacConfigNote` root.**
   `AggLayerBridge::allowed_notes()` includes it since protocol 0.17, and the
   proxy's builder extends that set, so a bridge created by a ≥0.17 proxy has it.
   A bridge created by 0.16 code may not; the one-shot then exits with the
   root to add. Adding it is a `NetworkAccountConfigNote`
   (`AddAllowedNoteScript`) from `service`, a separate reviewed change.
2. **The note is created and submitted by `service`**, whose key is in the KMS.
   The one-shot rebuilds and verifies the signer bindings exactly like a serving
   startup (`init::verify_remote_bindings`) before signing anything.

On a fee-charging chain, `service` pays the fee of the transaction that creates
the note and the **bridge pays the fee of consuming it** from its own vault, so
both vaults must be non-empty (step 2.3).

The target id does not need to exist on-chain yet: `grant_role` only validates
the id's structure. `0x93a6e276fa374b417db5b93f716e0d` is a valid 15-byte id
(version 1, private-type bit). Ask the requester for the bech32 form on the
target network (`mtst1…` on testnet) and check it decodes to the same hex before
granting; a wrong id is a grant to nobody that only an `ADMIN` can undo.

## 3. The tool

`miden-agglayer-service --grant-bridge-role <ROLE>=<ACCOUNT>` (and
`--revoke-bridge-role`) is a one-shot in the proxy binary, in the same family as
`--print-funding`: it needs the production `--miden-store-dir` (for
`bridge_accounts.toml`: the `service` and `bridge` ids), `--miden-node`, and the
custody flags `--signer-url` + `--signer-key …`. It does not start the RPC
server, the projector or any listener. Steps, in order:

1. parse `<ROLE>=<ACCOUNT>` (role from the fixed set above, case-insensitive;
   account as 0x-hex or bech32);
2. refuse a revoke of the `service` account;
3. `verify_remote_bindings`: the deployed `service` / `ger_manager` auth keys
   must be the ones `--signer-key` names, and the signer must still hold them;
4. sync, force-refresh the bridge from the node, and check: the allowlist holds
   the `RbacConfigNote` root; `service` holds the role's effective admin role;
   for a grant the account is not already a member (then it exits "already
   applied" without submitting); for a revoke the account is a member and is
   not the last one;
5. build the note, submit the `service` transaction through
   `submit_new_transaction` (so `--read-only` and the writer metrics apply),
   wait for it to commit;
6. poll the bridge's `role_membership` map (default 300 s,
   `--bridge-role-wait-secs`, `BRIDGE_ROLE_WAIT_SECS`) until the change is
   visible, then log `member_count_after`. A timeout is a warning, not a
   failure: the note is on-chain and the bridge will consume it.

`bridge-out-tool` cannot do this: it is local-custody only and cannot sign as
the KMS-keyed `service`.

The bridge-consumed-note detector (`unknown_wrapper_detector::canonical_admin_note_roots`)
counts the RBAC, pause and fee-policy config notes as canonical admin notes, so
the bridge consuming the grant does **not** fire `bridge_unknown_wrapper_consumed_total`
on a proxy that includes this change. On an older proxy it would fire exactly once.

## 4. Runbook

Variables, resolved from the live deployment (see `docs/operations/README.md`,
"Discover a Kubernetes deployment"): `$NAMESPACE`, `$WORKLOAD`, `$POD`,
`$MIDEN_NODE` (the node gRPC URL the proxy uses), `$STORE_DIR` (the proxy's
`--miden-store-dir`), `$AGGLAYER_SIGNER_URL`, the two `--signer-key` bindings,
`$NEW_ADMIN` (`0x93a6e276fa374b417db5b93f716e0d`).

### Step 1. Approval and identity

- Change ticket naming the requester, the account id, the network, and the fact
  that this grants full bridge administration (unpause, grant/revoke of every
  role) to that key.
- Requester confirms they control `$NEW_ADMIN` and provides its bech32 form for
  the target network. Decode both ways and compare the hex. Do not proceed on a
  bare hex pasted into chat.

### Step 2. Preflight (read-only, proxy still running)

2.1 **Current admin and bridge ids** from the proxy's config, no secrets printed:

```bash
kubectl -n "$NAMESPACE" exec "$POD" -- cat "$STORE_DIR/bridge_accounts.toml"
# service = "…"  bridge = "…"  ger_manager = "…"
```

2.2 **Fees and signer health** on the proxy's metrics endpoint:

```
remote_signer_verified_accounts          # expect 2
remote_signer_signature_failures_total   # not increasing
bridge_fee_vault_balance{account="service"}   # > 0: pays the note-creating tx
bridge_fee_vault_balance{account="bridge"}    # > 0: pays the note consumption
bridge_fee_vault_txns_left{…}                 # comfortably above 1 for both
```

`bridge-out-tool --vault-balance <service> <bridge> --node-url $MIDEN_NODE
--store-dir <throwaway>` gives the same numbers without the proxy.

2.3 **No other RBAC note is pending** for the bridge. Role notes are consumed
by the ntx-builder in an order it chooses; never have two in flight.

2.4 `AGGLAYER_READ_ONLY` is not set in the environment the one-shot will run in
(`submit_new_transaction` refuses mutations in read-only mode).

### Step 3. Stop the serving proxy

The Miden sqlite store and the projector are single-owner (runbook, "One
owner"). Scale the workload to zero and wait for the pod to be gone before the
one-shot opens the store:

```bash
kubectl -n "$NAMESPACE" scale "$WORKLOAD" --replicas=0
kubectl -n "$NAMESPACE" wait --for=delete pod/"$POD" --timeout=120s
```

Bridge-in/out traffic queues on the node and the aggkit side for the duration
(a few minutes). Announce the window.

### Step 4. Execute the one-shot

Run the same image, with the same store volume mounted and the same custody
environment as the serving pod (a `kubectl run`/`debug` pod, or a Job cloned
from the workload spec with the command overridden). The only differences from
the serving command line are the flag and that no ports are needed:

```bash
miden-agglayer-service \
  --miden-store-dir "$STORE_DIR" \
  --miden-node "$MIDEN_NODE" \
  --signer-url "$AGGLAYER_SIGNER_URL" \
  --signer-key service=<identifier> --signer-key ger-manager=<identifier> \
  --grant-bridge-role "ADMIN=$NEW_ADMIN"
```

(`AGGLAYER_SIGNER_URL`, `AGGLAYER_SIGNER_KEYS`, `MIDEN_PROVER_URL` and
`MIDEN_API_KEY` from the pod's environment are honoured, so with the
workload's env the flags reduce to `--miden-store-dir`, `--miden-node` and
`--grant-bridge-role`.)

Expected log lines, in order: `remote custody verified … accounts=2`,
`submitting bridge role change` (with the note id and `member_count_before=1`),
`role-change transaction submitted`, then within the wait budget
`bridge role change confirmed on-chain` with `member_count_after=2`, and
finally `bridge role change: note submitted and confirmed on the bridge`.

Possible exits before anything is submitted (all safe to re-run after fixing
the cause): unknown role, unparsable account, `bridge_accounts.toml` missing,
signer binding mismatch, allowlist missing the RBAC root, `service` not holding
`ADMIN`, account already a member (exit 0, "already applied").

If the run ends with `NOT yet consumed by the bridge (pending)`: the note exists
on-chain and will execute when the bridge consumes it. Do **not** re-run the
grant. Check the ntx-builder ("ntx-builder silent death" in the runbook) and
the bridge's fee vault, then confirm with step 6 later.

### Step 5. Restart the proxy

```bash
kubectl -n "$NAMESPACE" scale "$WORKLOAD" --replicas=1
```

Watch the normal startup checklist (runbook §2); `/health` 200,
`remote_signer_verified_accounts` 2. The proxy's own log will show the bridge
consuming a canonical admin note; `bridge_unknown_wrapper_consumed_total` must
not move on a build that includes `bridge_roles`.

### Step 6. Confirm and record

Re-running the exact same `--grant-bridge-role` command (it is idempotent) must
log `account already holds the role` and exit 0. That is the proof the
membership map on-chain carries the new admin; it needs the proxy stopped
again, so prefer reading the map from a throwaway client instead (Appendix B).

Update the deployment inventory and the change ticket with: both admin ids, the
note id, the transaction id, and the pre/post `member_count`. Add the new
admin's key to the same rotation/incident list as the KMS keys. Do **not**
remove `service` from any role.

## 5. Rollback

```bash
miden-agglayer-service … --revoke-bridge-role "ADMIN=$NEW_ADMIN"
```

Same preconditions and the same stop/run/restart cycle. Wait for the grant to
be consumed before issuing the revoke; the two notes have no ordering
guarantee. The tool refuses to revoke `service` from any role and refuses to
revoke the last member of a role (a memberless `ADMIN` can never be
re-populated).

## Appendix A. Reading membership from the bridge account

The readers the one-shot uses are public in `src/bridge_roles.rs`
(`holds_role`, `member_count`, `effective_admin_role`,
`allows_rbac_config_note`). From a throwaway `miden-client` store against
`$MIDEN_NODE` (never the proxy's own store):

```rust
client.import_account_by_id(bridge_id).await?;
let bridge = Account::try_from(client.get_account(bridge_id).await?.unwrap())?;
let admin = RoleBasedAccessControl::admin_role();
let is_member = bridge_roles::holds_role(&bridge, &admin, new_admin)?;
let count = bridge_roles::member_count(&bridge, &admin)?;
```

## Appendix B. Flag and env reference (proxy)

| Purpose | Flag | Env |
|---|---|---|
| Grant a bridge role and exit | `--grant-bridge-role ROLE=ACCOUNT` | — |
| Revoke a bridge role and exit | `--revoke-bridge-role ROLE=ACCOUNT` | — |
| Confirmation wait after commit | `--bridge-role-wait-secs` (default 300, 0 = none) | `BRIDGE_ROLE_WAIT_SECS` |
| Miden store directory | `--miden-store-dir` | — |
| Node gRPC URL or `testnet` | `--miden-node` | — |
| Remote signer base URL | `--signer-url` | `AGGLAYER_SIGNER_URL` |
| Role→key bindings | `--signer-key service=<id> --signer-key ger-manager=<id>` | `AGGLAYER_SIGNER_KEYS` |
| Read-only guard (must be off) | `--read-only` | `AGGLAYER_READ_ONLY` |

Related: [provisioning.md](provisioning.md) (custody, roles, rotation),
[runbook.md](runbook.md) ("One owner", "ntx-builder silent death"),
[monitoring.md](monitoring.md).
