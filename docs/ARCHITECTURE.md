# miden-agglayer proxy — architecture & main flows

Current as of the `reopen-92-synthetic-indexer-redesign` line (SyntheticProjector
as the sole synthetic-event producer + note-visibility reconciler + direct
recovery). Supersedes `docs/architecture.png` (pre-redesign, outdated).

## Component architecture

```mermaid
flowchart TD
    subgraph L1["Ethereum L1"]
        BRIDGE_L1["zkEVM Bridge contract"]
        GERM_L1["GER manager (L1InfoTree)"]
    end

    subgraph AGG["AggLayer stack"]
        AGGORACLE["aggkit / aggoracle"]
        AGGSENDER["aggkit / aggsender"]
        AGL["agglayer (settlement)"]
        BSVC["bridge-service (deposit index API)"]
        AUTOCLAIM["bridge-autoclaim (L1 claimer)"]
    end

    subgraph PROXY["miden-agglayer proxy (single process)"]
        RPC["JSON-RPC service<br/>eth_* / zkevm_* / admin_*"]
        CLAIM["claim path<br/>handle_claim_asset → publish_claim<br/>(per-origin faucet lock #10,<br/>miden decimals=min(origin,8), reject &gt;26 #17)"]
        GER["GER path<br/>insert_ger (ger_manager)"]
        WW["writer worker (RD-940, optional)<br/>future-nonce queue"]
        MC["MidenClient ACTOR<br/>single thread, one select! loop:<br/>sync_state every ~5s + request queue<br/>process-wide singleton"]
        subgraph LISTENERS["on_post_sync listeners"]
            SP["SyntheticProjector — SOLE producer<br/>cursor: Miden block N → synthetic block N<br/>+ note reconciler (sync_notes → import)<br/>+ late-consumption sweep<br/>+ direct recovery (get_notes_by_id +<br/>sync_transactions consumer attribution)"]
            BOS["BridgeOutScanner — monitors only<br/>LET divergence #9, twin notes #6,<br/>faucet ownership #4, expected MINT #7"]
        end
        STORE[("Store (Postgres / in-memory)<br/>synthetic blocks+logs, receipts,<br/>faucet registry, deposit_count,<br/>projector cursor")]
        SQLITE[("miden-client store.sqlite3<br/>+ keystore (proxy-private)")]
    end

    subgraph MIDEN["Miden network"]
        NODE["miden-node (sequencer RPC :57291)"]
        NTX["ntx-builder (consumes network notes)"]
        PROVER["tx-prover (remote proving)"]
        BACC["bridge account (LET, GER chain)"]
    end

    EXTW["external wallet client<br/>(independent store — e.g. bridge-out-tool)"]

    AGGORACLE -- "GER tx via eth_sendRawTransaction" --> RPC
    AUTOCLAIM -- "claimAsset via eth_sendRawTransaction" --> RPC
    AGGSENDER -- "eth_getLogs (BridgeEvent/ClaimEvent/GER)" --> RPC
    BSVC -- "indexes L1 + synthetic L2 logs" --> RPC
    RPC --> CLAIM & GER
    CLAIM & GER --> WW --> MC
    MC <--> SQLITE
    MC -- "gRPC (sync, submit, sync_notes,<br/>get_notes_by_id, sync_transactions)" --> NODE
    MC -- "prove tx" --> PROVER
    MC -- "on_post_sync" --> LISTENERS
    SP --> STORE
    NTX --> BACC
    EXTW -- "B2AGG note (own store, own keys)" --> NODE
    AGGSENDER --> AGL --> BRIDGE_L1
    AUTOCLAIM --> BRIDGE_L1
    GERM_L1 --> AGGORACLE

    classDef proxyBox fill:#dbeafe,stroke:#1d4ed8,stroke-width:2px
    classDef proxyNode fill:#eff6ff,stroke:#1d4ed8
    classDef aggBox fill:#e9d5ff,stroke:#7e22ce
    classDef aggNode fill:#f3e8ff,stroke:#7e22ce
    classDef midenBox fill:#ffedd5,stroke:#ea580c
    classDef midenNode fill:#fff7ed,stroke:#ea580c
    class PROXY,LISTENERS proxyBox
    class RPC,CLAIM,GER,WW,MC,SP,BOS,STORE,SQLITE proxyNode
    class L1,AGG aggBox
    class BRIDGE_L1,GERM_L1,AGGORACLE,AGGSENDER,AGL,BSVC,AUTOCLAIM aggNode
    class MIDEN midenBox
    class NODE,NTX,PROVER,BACC,EXTW midenNode
```

Key invariants:
- **One `MidenClient`** per process (guarded); all Miden work — sync, claims,
  GER, proving — serializes through its single loop. This is the throughput
  ceiling (~1 proven tx/min) and why the projector needs recovery paths for
  notes whose whole lifecycle fits between two sync points.
- **The projector is the only writer** of synthetic blocks/logs and the tip
  (`Miden block N ⇒ synthetic block N`, write-before-advance — a block is never
  exposed before its events are written, so `eth_getLogs` consumers cannot skip
  events; recovered events land in the first not-yet-exposed block).
- The external wallet **never shares the proxy's sqlite** (prod topology; also
  the DB-lock isolation result).

## The synthetic block engine (projector tick)

How synthetic blocks come to exist at all: the proxy has no EVM execution — the
projector *derives* an EVM-shaped chain from Miden, one synthetic block per
Miden block (Miden-1:1), inside every sync tick:

```mermaid
sequenceDiagram
    box rgb(255,237,213) Miden
        participant N as miden-node
    end
    box rgb(219,234,254) proxy
        participant MC as MidenClient actor
        participant SP as SyntheticProjector
        participant BS as BlockState
        participant ST as Store (synthetic chain)
    end

    MC->>N: sync_state (~5s cadence)
    N-->>MC: block headers, account deltas,<br/>tag-matched notes, spent nullifiers
    MC->>SP: on_post_sync (exclusive client access)
    SP->>N: reconciler: sync_notes + import missing<br/>+ direct recovery (R1 ladder)
    SP->>MC: get_input_notes(Consumed) — ONCE per tick
    SP->>SP: group consumed notes by consumption block<br/>merge late-swept + recovered notes into<br/>first unprojected block
    loop for each Miden block B = cursor+1 … tip
        SP->>BS: block hash + timestamp for B<br/>(derived from the Miden block)
        SP->>SP: order block notes by<br/>(consumed_tx_order, note_id) — deterministic
        SP->>ST: derive + write logs at synthetic block B:<br/>B2AGG→BridgeEvent, CLAIM→ClaimEvent,<br/>GER→UpdateHashChainValue<br/>(each gated + deduped — idempotent)
        SP->>ST: persist projector cursor = B
        SP->>ST: advance tip: latest_block_number = B<br/>(WRITE-BEFORE-ADVANCE — even for empty blocks)
    end
    Note over SP,ST: tip is the sole gate for eth_blockNumber /<br/>eth_getLogs readers: a block is never visible<br/>before all its events are written, so consumers<br/>cannot skip events — and re-running the projector<br/>over the same chain is byte-identical (deterministic)
```

Properties: **deterministic** (same Miden chain ⇒ byte-identical synthetic
chain), **idempotent** (crash mid-block re-projects through dedup keys),
**gap-free** (empty Miden blocks produce empty synthetic blocks, so the chain
mirrors Miden block-for-block and `eth_blockNumber` tracks the Miden tip).

## Flow 1 — GER injection (L1 → L2 info propagation)

```mermaid
sequenceDiagram
    box rgb(233,213,255) AggLayer / L1
        participant L1 as L1 (GER manager)
        participant AO as aggoracle
    end
    box rgb(219,234,254) proxy
        participant RPC as proxy RPC
        participant MC as MidenClient actor
        participant SP as SyntheticProjector
        participant ST as Store
    end
    box rgb(255,237,213) Miden
        participant N as miden-node / ntx-builder
    end

    L1->>AO: UpdateL1InfoTree (new GER)
    AO->>RPC: eth_sendRawTransaction (GER update tx)
    RPC->>MC: insert_ger via actor request queue
    MC->>MC: ger_manager signs UpdateGerNote (targets bridge)
    MC->>N: submit proven tx (tx-prover)
    N->>N: ntx-builder consumes UpdateGerNote<br/>bridge account: GER hash-chain += GER (block B)
    MC->>MC: sync_state sees consumption
    MC->>SP: on_post_sync
    SP->>ST: project_ger_note → UpdateHashChainValue log<br/>at synthetic block B (GER contract address)
    Note over SP,ST: dedup: is_ger_injected — idempotent
    ST-->>RPC: eth_getLogs / eth_getBlockByNumber
```

## Flow 2 — Claim (L1 → L2 deposit delivery)

```mermaid
sequenceDiagram
    box rgb(233,213,255) AggLayer / L1
        participant U as user / autoclaim
        participant BS as bridge-service
    end
    box rgb(219,234,254) proxy
        participant RPC as proxy RPC
        participant MC as MidenClient actor
        participant SP as SyntheticProjector
    end
    box rgb(255,237,213) Miden
        participant P as tx-prover
        participant N as miden-node / ntx-builder
        participant W as recipient wallet
    end

    U->>BS: (after L1 bridgeAsset) poll ready_for_claim
    Note over BS: deposit ready once its GER<br/>reached L2 via Flow 1
    U->>RPC: claimAsset via eth_sendRawTransaction
    RPC->>RPC: verify SMT proof vs injected GER<br/>find_or_create_faucet (per-origin lock #10,<br/>miden decimals=min(origin,8), reject &gt;26 #17)
    RPC->>MC: publish_claim via actor request queue<br/>(nonce guard R4 — writer worker queues future nonces)
    MC->>P: prove CLAIM note tx (~30–60 s)
    MC->>N: submit CLAIM (targets bridge)
    N->>N: ntx-builder consumes CLAIM (block B)<br/>bridge mints → MINT note to wallet
    MC->>SP: on_post_sync (consumption seen)
    SP->>SP: project_claim_note → ClaimEvent at block B<br/>claim receipt finalised by projector
    W->>N: consume MINT (P2ID tag) — funds on Miden
```

## Flow 3 — B2AGG (L2 → L1 bridge-out)

```mermaid
sequenceDiagram
    box rgb(255,237,213) Miden side
        participant EW as external wallet client
        participant N as miden-node / ntx-builder
    end
    box rgb(219,234,254) proxy
        participant MC as MidenClient actor
        participant SP as SyntheticProjector
    end
    box rgb(233,213,255) AggLayer / L1
        participant AS as aggsender / agglayer
        participant AC as bridge-autoclaim
        participant L1 as L1 bridge
    end

    EW->>N: submit B2AGG note (own store/keys,<br/>targets bridge, tag 0 network note)
    N->>N: ntx-builder consumes B2AGG (block B)<br/>bridge: burn asset, append LET leaf
    alt sync-visible (note seen before consumption)
        MC->>SP: consumed note in local store
        SP->>SP: project_b2agg_note → BridgeEvent at block B (exact)
    else missed by sync (fast lifecycle / sync starved)
        SP->>N: reconciler: sync_notes(range, tag 0)
        SP->>MC: import_notes(unknown ids)
        alt import lands (not yet consumed)
            MC->>SP: consumption discovered next sync → exact block B
        else spent-before-import (miden-client drops it)
            SP->>N: get_notes_by_id (full body)<br/>+ nullifier spend height<br/>+ sync_transactions(bridge) — consumer proof
            Note over SP: MA#3 gate: only bridge-executed<br/>consumptions emit (reclaims fail closed)
            SP->>SP: direct-project into first unexposed block
        end
    end
    SP->>SP: BridgeEvent(deposit_count++) — dedup by note id
    AS->>SP: eth_getLogs → build certificate → settle
    AC->>L1: claimAsset(exit) once settled
    L1-->>EW: funds to L1 recipient
```

## Verification harness

`scripts/e2e-bridge-loadtest-isolated.sh` (prod-faithful independent wallet)
ends with a 0-`database is locked` gate and
`scripts/verify-event-completeness.sh`: an independent cross-check of the
miden-node DB (consumed B2AGG/CLAIM/GER notes by canonical script root) against
`eth_getLogs` — **every consumed correct note must have exactly one event at
exactly its consumption block** (`late`/`missing`/`extra` reported per type).

## Recovery flows

Three distinct recovery mechanisms exist at different layers.

### R1 — Live note recovery ladder (event completeness, in-process)

Why: notes created by external wallets that are committed **and** consumed
between two proxy sync points are never delivered by interest-based
`sync_state`; under load (claims starving the actor loop) this window grows to
minutes. Three escalating catchers:

```mermaid
sequenceDiagram
    box rgb(255,237,213) Miden
        participant N as miden-node
    end
    box rgb(219,234,254) proxy
        participant MC as MidenClient store
        participant SP as SyntheticProjector (per tick)
    end

    Note over SP: Catcher 1 — late-consumption sweep
    SP->>MC: get_input_notes(Consumed)
    SP->>SP: note consumed at block ≤ cursor,<br/>not yet processed → project into<br/>FIRST unexposed block (tip never<br/>advances past unwritten events)

    Note over SP: Catcher 2 — note reconciler
    SP->>N: sync_notes(cursor+1 … tip, tag 0) — ≤200 blocks/tick
    SP->>MC: which ids unknown? (NoteFilter::List)
    SP->>MC: import_notes(NoteFile::NoteId …)
    MC->>N: fetch bodies + inclusion proofs
    Note over MC: next sync discovers the nullifier →<br/>consumed state → Catcher 1 projects it

    Note over SP: Catcher 3 — direct recovery (spent-before-import)
    SP->>MC: re-query: which imports did NOT land?<br/>(miden-client 0.15 silently drops<br/>already-spent imports)
    SP->>N: get_notes_by_id (full public body)
    SP->>N: nullifier spend height
    SP->>N: sync_transactions(bridge account, spend range)
    Note over SP: MA#3 reclaim gate: emit ONLY if a<br/>bridge-executed tx consumed this nullifier<br/>(reclaim by sender ⇒ fail-closed skip + metric)
    SP->>SP: fabricate ConsumedExternal record →<br/>same project_b2agg_note derivation,<br/>dedup by note id, retry-safe queue
```

### R2 — Startup restore (disaster recovery, `--restore`)

Rebuilds the synthetic event store from Miden after data loss
(`--reset-miden-store --restore` wipes the sqlite first; Postgres dedup keys
make the replay idempotent):

```mermaid
sequenceDiagram
    box rgb(229,231,235) operator
        participant OP as operator
    end
    box rgb(219,234,254) proxy
        participant PX as proxy (startup)
        participant MC as MidenClient
        participant ST as Store
    end
    box rgb(255,237,213) Miden
        participant N as miden-node
    end

    OP->>PX: start with --restore [--reset-miden-store]
    PX->>PX: pause sync listeners (no live projection<br/>during replay)
    PX->>MC: Phase 1: sync_state to Miden tip
    MC->>N: full sync (accounts, tag-matched notes)
    PX->>ST: Phase 2: replay consumed B2AGGs →<br/>project_b2agg_note (BridgeEvents, deposit_count)
    PX->>ST: Phase 3: replay consumed UpdateGerNotes →<br/>GER set + hash chain
    PX->>ST: Phase 4: replay consumed CLAIMs →<br/>ClaimEvents (MA#27 synthesis)
    PX->>PX: unpause listeners — projector cursor<br/>resumes catch-up as the normal loop
    Note over PX,ST: limitation: replay reads the LOCAL consumed-note<br/>view — notes invisible to sync are healed by the<br/>R1 reconciler's genesis re-sweep after startup
```

### R3 — Account self-heal (runtime, per-submission)

```mermaid
sequenceDiagram
    box rgb(219,234,254) proxy
        participant CP as claim / GER path
        participant MC as MidenClient
    end
    box rgb(255,237,213) Miden
        participant N as miden-node
    end

    CP->>MC: submit tx (insert_ger / publish_claim)
    MC-->>CP: AccountDataNotFound /<br/>IncorrectAccountInitialCommitment
    CP->>MC: import_account_by_id(affected account)
    MC->>N: fetch live PUBLIC account state
    CP->>MC: retry submission once
    Note over CP: infra accounts are deployed PUBLIC precisely<br/>so a lost sqlite row is recoverable from chain
```
