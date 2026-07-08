# Code-health tooling — static & dynamic analysis

A tailored catalogue of static and dynamic analysis tools for keeping this
codebase (and its architecture) healthy. Prioritised and mapped to **this
repo's actual risk profile**: a concurrency-heavy tokio actor over a shared
SQLite store, an EVM↔Miden **bridge that handles funds** and decodes untrusted
on-chain metadata, on-disk **keystores + API keys**, and a `postgres` cargo
feature.

Legend: **[HAVE]** already wired in CI/Makefile · **[ADD]** recommended gap ·
**[SITUATIONAL]** adopt when the matching need appears.

---

## 0. What's already in place (credit where due)

CI (`.github/workflows/check.yml`) → `make lint && make build && make test-unit`:

| Tool | Role | Notes |
|---|---|---|
| **clippy** `-D warnings` | Rust linter | `make clippy`, all-targets, workspace. Strong baseline. |
| **rustfmt** `--check` | formatting | `make format-check` |
| **taplo** | TOML formatting | `make toml-check` |
| **typos** | spell-check | `make typos-check` |
| unit + e2e tests | correctness | `make test-unit`, `make test-e2e` (docker stack) |
| Swatinem/rust-cache | CI cache | — |

Toolchain is **stable 1.96.0**, no nightly pinned. Several tools below need a
nightly component (sanitizers, Miri) — add it as a separate CI job, don't move
the main build off stable.

The gaps that matter most here, in order: **(1)** no supply-chain/advisory
scanning, **(2)** no architectural lint enforcing the "single canonical SQLite
opener" invariant, **(3)** the `postgres` feature is never built in CI, **(4)**
no fuzz/property testing of the untrusted-metadata decode paths, **(5)** no
secret scanning despite keystores in-tree, **(6)** no coverage signal.

---

## 1. Static analysis — Rust source

### Tier 1 — adopt now (cheap, high value)

| Tool | [status] | Why it matters *here* | Command / wiring |
|---|---|---|---|
| **cargo-deny** | [ADD] | Big dependency tree (alloy, miden-\*, tokio, postgres). Enforces advisories + license policy + **banned crates** + duplicate-version bans in one gate. | `cargo deny check` (add `deny.toml`); CI job |
| **cargo-audit** | [ADD] | RustSec advisory scan of `Cargo.lock`. You pin `=0.15.0` exact versions — advisories still land against pinned versions. | `cargo audit`; or rely on `cargo deny check advisories` |
| **clippy `disallowed_methods`/`disallowed_types`** | [ADD] | **Architectural fitness function.** Ban `rusqlite::Connection::open` and raw `ClientBuilder::…sqlite_store` *outside* `sqlite_pragmas`/`miden_client` — the exact footgun behind the DB-lock work (multiple un-pragma'd store openers). Turns a code-review rule into a compile error. | `clippy.toml` (see §6) |
| **cargo-hack** | [ADD] | The **`postgres` feature is not built in CI** — `make build` uses defaults. `--feature-powerset` builds every feature combo; catches `#[cfg(feature="postgres")]` rot. | `cargo hack check --feature-powerset --no-dev-deps` |
| **cargo-machete** | [ADD] | Fast unused-dependency finder (pure-manifest, no build). Trims attack surface + build time. | `cargo machete` |
| **workspace `[lints]` table** | [ADD] | Promote a lint policy into `Cargo.toml` (`unsafe_code = "forbid"` where possible, `rust_2018_idioms`, select `clippy::pedantic`) so it's declarative and inherited, not just CLI flags. | `[lints.rust]` / `[lints.clippy]` in `Cargo.toml` |

### Tier 2 — high value, slightly more setup

| Tool | [status] | Why here | Notes |
|---|---|---|---|
| **cargo-udeps** | [SITUATIONAL] | Deeper unused-dep detection than machete (compiles, finds unused *by the build graph*). Nightly. | `cargo +nightly udeps --all-targets` |
| **cargo-semver-checks** | [SITUATIONAL] | Only if any crate here is consumed as a **library** with a stability contract. Detects breaking API changes. | pairs with release.yml |
| **cargo-outdated** | [SITUATIONAL] | Surface newer deps; you pin exact versions, so this is an intentional-upgrade aid, not a gate. | `cargo outdated -R` |
| **cargo-geiger** | [SITUATIONAL] | Counts `unsafe` across the dep tree. Useful for a funds-handling service to keep an eye on unsafe surface. | `cargo geiger` |

### SAST / cross-language

| Tool | [status] | Why here |
|---|---|---|
| **CodeQL (Rust)** | [ADD] | GitHub-native SAST; Rust support is now GA. Data-flow analysis over a **bridge handling value + untrusted input** is exactly its sweet spot. Add `github/codeql-action` workflow. |
| **Semgrep** | [SITUATIONAL] | Lightweight custom rules (e.g. "no `unwrap()` on network/DB paths", "no `println!` in library code"). Faster to author bespoke rules than CodeQL. |

---

## 2. Dynamic analysis — runtime

### Concurrency & the SQLite store (this repo's hot spot)

| Tool | [status] | Why here | How |
|---|---|---|---|
| **tokio-console** | [ADD] | The `MidenClient` actor is a single-threaded `select!` loop; the whole DB-lock investigation is "what is blocking whom." tokio-console shows **stalled tasks, long polls, and resource waits live** — it would visualise the sync task holding the runtime while a claim waits. | add `console-subscriber`, run behind a `tokio-console` cargo feature |
| **Loom** | [ADD] | Exhaustive interleaving model-checker for **targeted** concurrency units. Model the actor's request/sync interleaving and `writer_worker` state machine under `cfg(loom)`. Catches ordering bugs a load test only hits probabilistically. | `loom` dev-dep, `#[cfg(loom)]` tests |
| **ThreadSanitizer (TSan)** | [ADD] | The Go `-race` analog — runtime **data-race** detector. Note: **does not** catch SQLite `database is locked` (that's DB-layer lock contention, not a memory race); complements it for the proxy's own `Arc`/atomic/actor code. | nightly + `-Zbuild-std`; `RUSTFLAGS="-Zsanitizer=thread"`; `TSAN_OPTIONS="halt_on_error=0 exitcode=0"` to **report without stopping** |
| **parking_lot `deadlock_detection`** | [SITUATIONAL] | If/where `parking_lot` mutexes are used, its background deadlock detector reports lock cycles. | enable the feature in test/bench builds |
| **SQLite diagnostics** | [ADD] | For the lock class specifically: `PRAGMA integrity_check`, `PRAGMA busy_timeout`, `journal_mode` assertions, and slow-statement logging. A tiny startup self-check that the store is in the expected journal mode prevents silent regressions. | rusqlite `pragma_query` at boot |

### Memory / UB / leaks

| Tool | [status] | Why here | How |
|---|---|---|---|
| **Miri** | [ADD] | Detects UB, data races, and strict-provenance violations in **unit tests** (interpreter — can't run the networked stack, but perfect for pure logic: decimal scaling, note/metadata parsing). | `cargo +nightly miri test` (subset) |
| **AddressSanitizer / LeakSanitizer** | [SITUATIONAL] | Heap/UAF/leak detection under load. Most relevant around the bundled **SQLite C** and any FFI. | nightly `-Zsanitizer=address`/`leak` |
| **valgrind (memcheck/massif) / heaptrack / dhat** | [SITUATIONAL] | Heap-profile the proxy under the load test — catch slow leaks / growth in the long-running actor. `dhat` integrates as a Rust allocator. | run the loadtest against an instrumented build |

### Fuzzing & property testing (untrusted-input paths)

| Tool | [status] | Why here | Target |
|---|---|---|---|
| **cargo-fuzz (libFuzzer)** | [ADD] | **Finding #17 was a malicious-metadata bug** (`abi_decode_string`, decimal bounds). Coverage-guided fuzzing of the metadata/note/RPC decoders is the highest-leverage dynamic tool for a bridge. | `parse_token_metadata`, B2AGG/note decode, ABI decode |
| **proptest** (or quickcheck) | [ADD] | Property tests for **invariants** — e.g. the decimal-scaling envelope from finding #17 (`m ≤ 12 ∧ d−m ≤ 18`), amount round-trips, nonce ordering. Cheaper than fuzzing, runs in normal CI. | add as dev-dep |
| **AFL++** | [SITUATIONAL] | Alternative fuzzer if libFuzzer coverage plateaus. | same targets |

### Coverage & test runner

| Tool | [status] | Why here | How |
|---|---|---|---|
| **cargo-llvm-cov** | [ADD] | Accurate line/branch coverage (LLVM source-based). Baseline the security-critical modules (`claim`, `faucet_ops`, `service_send_raw_txn`, `miden_client`). | `cargo llvm-cov --lcov`; upload to Codecov/report artifact |
| **cargo-nextest** | [ADD] | Faster parallel test runner with **flaky-retry**, per-test timeouts, and partitioning. Given a concurrency-sensitive suite, retry+timeout visibility is worth it. | `cargo nextest run` |

---

## 3. Architecture & structure

There is no ArchUnit-grade framework for Rust, but the practical equivalent is
a combination of **module privacy + declarative bans + graph inspection**:

| Tool | [status] | Role |
|---|---|---|
| **clippy `disallowed_*` (clippy.toml)** | [ADD] | The real "architecture fitness function" for Rust — enforce layering/ownership rules as lints (e.g. only `sqlite_pragmas` may open a raw store connection; only the actor may hold `MidenClientLib`). See §6. |
| **cargo-modules** | [SITUATIONAL] | Visualise the module tree + inter-module dependencies; spot cycles and boundary violations. `cargo modules dependencies`. |
| **cargo tree -d / cargo-guppy** | [ADD] | `cargo tree -d` finds **duplicate dependency versions** (bloat + subtle type-mismatch pain). guppy does deeper workspace graph queries. |
| **cargo-depgraph** | [SITUATIONAL] | Graphviz of the dependency graph for docs/review. |
| **rust-code-analysis / tokei / cargo-bloat** | [SITUATIONAL] | Complexity metrics (cognitive/cyclomatic), LOC, and binary-size attribution (`cargo bloat`) to spot hotspots and creep. |
| **cargo-public-api** | [SITUATIONAL] | Snapshot the public API surface; diff in CI to catch unintended exposure. |

---

## 4. Security-specific (bridge + keys)

| Tool | [status] | Why here |
|---|---|---|
| **gitleaks** and/or **trufflehog** | [ADD] | The repo handles **keystores + `ADMIN_API_KEY` + prover URLs**; code comments explicitly warn "NEVER hardcode a key." Add a secret-scan CI job **and** a pre-commit hook — catch a leaked key *before* it's pushed. |
| **cargo-deny (advisories + licenses + sources)** | [ADD] | (also §1) Supply-chain gate: known-vuln deps, disallowed licenses, and **crate-source pinning** (only crates.io + your vendor) to blunt dependency-confusion. |
| **cargo-vet** | [SITUATIONAL] | Records human audits of dependencies; strong for a funds-handling service that wants provenance on every third-party crate. Heavier process — adopt when the team is ready. |
| **CodeQL / Semgrep** | [ADD/SIT] | (also §1) Taint analysis from untrusted on-chain inputs to sensitive sinks. |
| **cargo-fuzz on decoders** | [ADD] | (also §2) The single most valuable *security* dynamic tool here — untrusted bytes → parser. |

---

## 5. Suggested CI roadmap (incremental)

Add as **separate jobs** so a heavyweight one failing doesn't block the fast lane.

1. **Now (fast, stable):** `cargo deny check` · `cargo hack check --feature-powerset --no-dev-deps` · `cargo machete` · gitleaks scan. Fold into `make lint` where cheap.
2. **Next:** `cargo llvm-cov` (report + threshold on critical modules) · `cargo nextest` as the runner · proptest invariants in `test-unit`.
3. **Nightly scheduled (heavy):** Miri (unit subset) · TSan run of the load test (report-only) · `cargo fuzz run` with a time budget · CodeQL.
4. **On demand / investigation:** tokio-console + Loom for the actor; dhat/heaptrack under the load test.

A **pre-commit** config (`rustfmt`, `clippy`, `typos`, `taplo`, `gitleaks`)
shifts the fast checks left so they don't burn CI minutes.

---

## 6. Concrete starters

### `clippy.toml` — enforce the single-store-opener invariant

```toml
# Only crate::sqlite_pragmas may open a raw miden store connection. Everything
# else must go through the canonical opener (which sets the required pragmas).
# This is the compile-time guard for the DB-lock class of bug.
disallowed-methods = [
  { path = "rusqlite::Connection::open", reason = "use crate::sqlite_pragmas::open_store_connection" },
]
```

*(Allow the two legitimate call sites with a scoped `#[allow(clippy::disallowed_methods)]` + a comment, so any NEW opener trips CI.)*

### `deny.toml` — supply-chain gate (skeleton)

```toml
[advisories]
yanked = "deny"
[bans]
multiple-versions = "warn"       # tighten to "deny" once duplicates are resolved
[licenses]
allow = ["MIT", "Apache-2.0", "BSD-3-Clause", "Unicode-3.0"]  # adjust to policy
[sources]
unknown-registry = "deny"        # only crates.io + declared sources
```

### TSan load-test run (report-only)

```bash
RUSTFLAGS="-Zsanitizer=thread" \
  cargo +nightly build -Zbuild-std --target x86_64-unknown-linux-gnu --bin miden-agglayer-service
TSAN_OPTIONS="halt_on_error=0 exitcode=0 log_path=./tsan" \
  ./target/x86_64-unknown-linux-gnu/debug/miden-agglayer-service ...   # then run the load test
```

### proptest invariant (finding #17 shape)

```rust
proptest! {
    #[test]
    fn faucet_route_is_always_satisfiable(d in 0u8..=26) {
        let m = d.min(8);                 // cap-at-8: miden_decimals = min(origin, 8)
        prop_assert!(m <= 12);            // MAX_MIDEN_DECIMALS (faucet builder cap)
        prop_assert!(d - m <= 18);        // MAX_SCALING_FACTOR ⇒ origin_decimals <= 26
    }
}
// Every d <= 26 routes (d <= 8 at scale 0, so 6-decimal USDC/USDT work); only
// d > 26 (scale > 18) has no route and is rejected up-front (finding #17,
// audit-aligned "reject >26").
```

---

*Not exhaustive by intent — prioritised. The through-line: this is a
concurrency-heavy, funds-handling bridge with untrusted input and secrets on
disk, so the highest-leverage additions are supply-chain gating (cargo-deny),
architectural lints (clippy disallowed), fuzz/property testing of decoders, and
concurrency introspection (tokio-console/Loom/TSan).*
