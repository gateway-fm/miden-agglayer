.DEFAULT_GOAL := help

CARGO_PROFILE ?= dev
CARGO_RELEASE_ARG := $(if $(filter release,$(CARGO_PROFILE)),--release,)

.PHONY: help
help: ## Show description of all commands
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# --- Linting -------------------------------------------------------------------------------------

.PHONY: clippy-fix
clippy-fix: ## cargo clippy: fix problems if possible
	cargo clippy --workspace --profile=$(CARGO_PROFILE) --all-targets --quiet --fix --allow-dirty -- -D warnings

.PHONY: clippy
clippy: ## cargo clippy
	cargo clippy --workspace --profile=$(CARGO_PROFILE) --all-targets --quiet -- -D warnings

.PHONY: format
format: ## Format Rust files
	cargo fmt --all

.PHONY: format-check
format-check: ## Check Rust files formatting
	cargo fmt --all --check

.PHONY: toml
toml: ## Format TOML files
	RUST_LOG=warn taplo fmt

.PHONY: toml-check
toml-check: ## Check TOML files formatting
	RUST_LOG=warn taplo fmt --check

.PHONY: typos-fix
typos-fix: ## Fix spelling mistakes
	typos --config ./.typos.toml --write-changes

.PHONY: typos-check
typos-check: ## Check spelling mistakes
	typos --config ./.typos.toml

.PHONY: fmt
fmt: format toml ## Format Rust and TOML files

.PHONY: lint-fix
lint-fix: format toml typos-fix clippy-fix ## Perform linting and fix problems if possible

.PHONY: lint
lint: format-check toml-check typos-check clippy ## Perform linting

# --- Documentation -------------------------------------------------------------------------------

.PHONY: doc
doc: ## Generate Rust docs
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo doc --lib --no-deps --all-features --keep-going --profile=$(CARGO_PROFILE)

.PHONY: doc-open
doc-open: ## Generate Rust docs and open a browser
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo doc --lib --no-deps --all-features --keep-going --profile=$(CARGO_PROFILE) --open

# --- Testing -------------------------------------------------------------------------------------

.PHONY: test
test: test-unit test-e2e ## Run everything: unit tests, then spin up stack and run E2E

.PHONY: test-unit
test-unit: ## Run unit tests (no docker needed)
	cargo test --workspace --profile=$(CARGO_PROFILE) --lib

.PHONY: test-e2e
test-e2e: ## Spin up docker stack, run E2E tests, tear down (fully self-contained)
	@echo "╔══════════════════════════════════════════════════════════════╗"
	@echo "║  Starting E2E stack (Anvil, Miden node, PG, bridge, aggkit) ║"
	@echo "╚══════════════════════════════════════════════════════════════╝"
	@$(MAKE) --no-print-directory e2e-clean-data
	@./scripts/ensure-e2e-secrets.sh
	$(E2E_COMPOSE) up -d --build --wait
	@echo ""
	@echo "Stack is up — running E2E tests..."
	@echo ""
	./scripts/e2e-test.sh; EXIT_CODE=$$?; \
		echo ""; \
		echo "Tearing down stack..."; \
		$(E2E_COMPOSE) down -v; \
		exit $$EXIT_CODE

.PHONY: test-nextest
test-nextest: ## Run unit tests via cargo-nextest (faster, needs install-tools)
	cargo nextest run --workspace $(CARGO_RELEASE_ARG) --lib --no-tests pass

.PHONY: test-postgres
test-postgres: ## Run PgStore integration tests (needs DATABASE_URL)
	cargo test --workspace --profile=$(CARGO_PROFILE) --features postgres --lib -- pgstore

.PHONY: test-docs
test-docs: ## Run documentation tests
	cargo test --doc --profile=$(CARGO_PROFILE)

# --- Integration testing -------------------------------------------------------------------------

NODE_DATA_DIR = ${HOME}/.miden/node-data

.PHONY: node-init
node-init: ## Bootstrap the test node
	rm -rf "$(NODE_DATA_DIR)"
	mkdir -p "$(NODE_DATA_DIR)"
	RUST_LOG=warn ../miden-node/target/release/miden-node bundled bootstrap --data-directory "$(NODE_DATA_DIR)" --accounts-directory "$(NODE_DATA_DIR)" --genesis-config-file ../miden-node/crates/store/src/genesis/config/samples/01-simple.toml

.PHONY: start-node
start-node: ## Start the test node
	if [[ ! -d "$(NODE_DATA_DIR)" ]]; then $(MAKE) node-init; fi
	RUST_LOG=info,miden_node_utils::tracing::grpc=off,miden_node_ntx_builder::builder=warn,miden_node_block_producer::batch_builder=warn,miden-block-producer=warn,miden_node_utils::lru_cache=off,miden-store=warn,miden_node_validator=warn,miden_node_ntx_builder::coordinator=warn,miden_node_ntx_builder::actor=warn,miden-ntx-builder=warn \
	../miden-node/target/release/miden-node bundled start --rpc.url "http://0.0.0.0:57291" --data-directory "$(NODE_DATA_DIR)"

.PHONY: stop-node
stop-node: ## Stop the test node
	@# -pkill -f "test_node"
	-pkill -f "miden-node"
	sleep 1

.PHONY: node
node: start-node ## Start the test node

# --- Building ------------------------------------------------------------------------------------

.PHONY: build
build: ## Build all the binaries
	cargo build --workspace --profile=$(CARGO_PROFILE)

.PHONY: check
check: ## cargo check: compile without producing binaries
	cargo check --workspace --profile=$(CARGO_PROFILE)

.PHONY: fix
fix: ## cargo fix: cargo check and fix warnings if possible
	cargo fix --workspace --profile=$(CARGO_PROFILE) --all-targets --allow-staged --allow-dirty

.PHONY: docker
docker: ## Build a docker image
	docker build . -t miden-infra/miden-proxy:latest

## --- Setup --------------------------------------------------------------------------------------

.PHONY: check-tools
check-tools: ## Check if development tools are installed
	@echo "Checking development tools..."
	@command -v mdbook        >/dev/null 2>&1 && echo "[OK] mdbook is installed"        || echo "[MISSING] mdbook       (make install-tools)"
	@command -v typos         >/dev/null 2>&1 && echo "[OK] typos is installed"         || echo "[MISSING] typos        (make install-tools)"
	@command -v cargo nextest >/dev/null 2>&1 && echo "[OK] cargo-nextest is installed" || echo "[MISSING] cargo-nextest(make install-tools)"
	@command -v taplo         >/dev/null 2>&1 && echo "[OK] taplo is installed"         || echo "[MISSING] taplo        (make install-tools)"

.PHONY: install-tools
install-tools: ## Install development tools
	@echo "Installing development tools..."
	@rustup show active-toolchain >/dev/null 2>&1 || (echo "Rust toolchain not detected. Install rustup + toolchain first." && exit 1)
	@RUST_TC=$$(rustup show active-toolchain | awk '{print $$1}'); \
		echo "Ensuring required Rust components are installed for $$RUST_TC..."; \
		rustup component add --toolchain $$RUST_TC clippy rustfmt >/dev/null
	# Rust-related
	cargo install mdbook --locked
	cargo install typos-cli --locked
	cargo install cargo-nextest --locked
	cargo install taplo-cli --locked
	@echo "Development tools installation complete!"

# --- E2E Testing (docker-compose, no Kurtosis) -------------------------------------------

# The local miden-node container is built from the production
# `0xMiden/miden-node` repo (NOT the miden-client testing-node-builder).
# Agglayer support moved out of the testing harness in v0.14.7+; the
# production miden-node binary now uses a `--genesis-config-file` TOML to
# load pre-built bridge / faucet `.mac` account files into genesis.
# miden-node-store's build.rs auto-generates those files deterministically
# from miden-agglayer's account builders, so the e2e image is fully
# reproducible.
#
# Protocol 0.15.x: the node repo was renamed `0xMiden/miden-node` ->
# `0xMiden/node` (package name stays `miden-node`, so `--bin miden-node` and
# the `bundled bootstrap/start` CLI are unchanged, and the genesis sample
# `02-with-account-files.toml` is at the same path). We pin to the exact rev
# the miden-client 0.15 branch (PR #2224) was built against, so the node's
# transitive miden-protocol 0.15.2 / miden-assembly 0.23.x match our
# Cargo.toml pins and the BURN/MINT/CLAIM MAST roots agree on both sides.
# When a stable v0.15.x miden-node tag ships, pin to the tag instead.
#
# Bumping: edit MIDEN_NODE_GIT_REF here. The build.args plumb it through
# docker-compose so the Dockerfile picks it up at build time.
MIDEN_NODE_GIT_URL := https://github.com/0xMiden/node.git
# v0.15.0 (final tag). Builds against miden-protocol/standards/tx 0.15.3 — the
# same base crates as our service — so BURN/MINT/CLAIM/B2AGG MAST roots agree
# across the node/client boundary with no Cargo.lock-alignment hack. The
# node-store callback-vault-key bug is fixed upstream at this tag (the buggy
# select_vault_balances_by_faucet_ids is gone), so fixtures/patches/0001 is no
# longer applied. Network id is now a runtime storage slot, so no vendor patch.
MIDEN_NODE_GIT_REF := v0.15.0

E2E_COMPOSE := MIDEN_NODE_GIT_URL=$(MIDEN_NODE_GIT_URL) MIDEN_NODE_GIT_REF=$(MIDEN_NODE_GIT_REF) docker compose -f docker-compose.e2e.yml --env-file fixtures/.env

.PHONY: miden-node-image-coords
miden-node-image-coords: ## Print the git URL + ref the miden-node image is built from
	@echo "url: $(MIDEN_NODE_GIT_URL)"
	@echo "ref: $(MIDEN_NODE_GIT_REF)"

.PHONY: e2e-setup
e2e-setup: ## One-time: extract Anvil snapshot + configs from Kurtosis
	./scripts/setup-fixtures.sh

.PHONY: e2e-clean-data
e2e-clean-data: ## Wipe .miden-agglayer-data/ + node_data volume so the stack re-inits against fresh genesis
	# Proto 0.15: the node is a microservice stack whose state (genesis block,
	# store, validator + ntx-builder DBs) lives in the `node_data` Docker volume,
	# and the proxy's genesis pin lives in .miden-agglayer-data/store.sqlite3.
	# Mounting stale node state under a fresh proxy (or vice-versa) makes the
	# client's sync fail ("accept header validation failed"), so wipe BOTH for a
	# clean slate. The bootstrap services rebuild genesis (deterministic from the
	# vendored agglayer .mac files) and the proxy's --init redeploys accounts in
	# ~45s — acceptable for E2E. The volume rm is guarded so it no-ops when the
	# volume is absent or still in use (containers must be down first, which the
	# regression harness guarantees via `make e2e-down`).
	rm -rf .miden-agglayer-data
	mkdir -p .miden-agglayer-data/tmp
	-docker volume rm miden-agglayer_node_data 2>/dev/null || true

.PHONY: e2e-up
e2e-up: e2e-clean-data ## Start full E2E environment (cleans data dir first)
	$(E2E_COMPOSE) up -d --build --wait

# The e2e scripts call `docker compose` directly (for one-shot runs,
# stop/start, etc.), so they need the MIDEN_NODE_GIT_{URL,REF} env vars
# the compose file requires. Each script-invoking target exports them
# explicitly. (Centralising in the script harness itself would mean every
# contributor remembers to source this — easier to inject here.)
COMPOSE_ENV := MIDEN_NODE_GIT_URL=$(MIDEN_NODE_GIT_URL) MIDEN_NODE_GIT_REF=$(MIDEN_NODE_GIT_REF)

.PHONY: e2e-test
e2e-test: ## Run E2E tests (assumes stack is already up)
	$(COMPOSE_ENV) ./scripts/e2e-test.sh

.PHONY: e2e-l1-to-l2
e2e-l1-to-l2: e2e-up ## Spin up stack + run L1→L2 deposit + claim test
	$(COMPOSE_ENV) ./scripts/e2e-l1-to-l2.sh

.PHONY: e2e-claim-watcher
e2e-claim-watcher: e2e-l1-to-l2 ## After L1→L2, assert the chain-tail CLAIM watcher fired (happy path: already_recorded)
	$(COMPOSE_ENV) ./scripts/e2e-claim-watcher.sh

.PHONY: e2e-claim-watcher-synthesis
e2e-claim-watcher-synthesis: e2e-claim-watcher ## After watcher happy path, simulate desync and assert synthesis fires (RD-860/EFAD repro)
	$(COMPOSE_ENV) ./scripts/e2e-claim-watcher-synthesis.sh

.PHONY: e2e-claim-provenance
e2e-claim-provenance: ## Deploy a FOREIGN bridge on the same chain, drive a claim through it, assert zero ClaimEvent leakage (stack must be up)
	$(COMPOSE_ENV) ./scripts/e2e-claim-provenance.sh

.PHONY: e2e-l2-to-l1
e2e-l2-to-l1: e2e-l1-to-l2 ## Spin up stack + L1→L2 to fund wallet + run L2→L1 bridge-out test (strict)
	$(COMPOSE_ENV) ./scripts/e2e-l2-to-l1.sh

.PHONY: e2e-l2-to-l1-best-effort
e2e-l2-to-l1-best-effort: e2e-l1-to-l2 ## L2→L1 with extended timeout + miden-node crash detection (exits 2 on upstream miden-node v0.14.10 crash-loop, 1 on real regression)
	$(COMPOSE_ENV) ./scripts/e2e-l2-to-l1-best-effort.sh

.PHONY: ensure-sponsor-key
ensure-sponsor-key: ## Decrypt claimsponsor.keystore -> SPONSOR_PRIVATE_KEY in fixtures/.env (for the Rust bridge-autoclaim)
	./scripts/ensure-sponsor-key.sh

.PHONY: e2e-l2-to-l1-autoclaim
# ensure-sponsor-key MUST precede e2e-l1-to-l2: the latter triggers e2e-up,
# which starts the bridge-autoclaim service reading SPONSOR_PRIVATE_KEY from
# fixtures/.env. Prerequisites are built left-to-right (serial make).
e2e-l2-to-l1-autoclaim: ensure-sponsor-key e2e-l1-to-l2 ## Spin up stack + L1→L2 to fund + L2→L1 bridge-out claimed automatically by the Rust bridge-autoclaim
	$(COMPOSE_ENV) ./scripts/e2e-l2-to-l1-autoclaim.sh

.PHONY: e2e-restore
e2e-restore: e2e-up ## Spin up stack + run disaster recovery restore test
	$(COMPOSE_ENV) ./scripts/e2e-restore.sh

.PHONY: e2e-reconciler-private-note
e2e-reconciler-private-note: e2e-up ## Spin up stack + reconciler private-note wedge regression (0.15.5 hotfix, PR #110)
	$(COMPOSE_ENV) ./scripts/e2e-reconciler-private-note.sh

.PHONY: e2e-ger-decomposition
e2e-ger-decomposition: e2e-up ## Spin up stack + run GER decomposition bug regression test
	$(COMPOSE_ENV) ./scripts/e2e-ger-decomposition.sh

.PHONY: e2e-security
e2e-security: e2e-up ## Spin up stack + run security E2E tests
	$(COMPOSE_ENV) ./scripts/e2e-security.sh

.PHONY: e2e-fuzz
e2e-fuzz: e2e-up ## Spin up stack + run bridge fuzz/stress tests
	$(COMPOSE_ENV) ./scripts/e2e-fuzz-bridge.sh

.PHONY: e2e-rd913-restart-burn-collision
e2e-rd913-restart-burn-collision: e2e-up ## Spin up stack + verify monitor state survives proxy restart (RD-913)
	$(COMPOSE_ENV) ./scripts/e2e-rd913-restart-burn-collision.sh

.PHONY: e2e-reconciler-cursor-persistence
e2e-reconciler-cursor-persistence: e2e-up ## Spin up stack + verify the reconciler sweep cursor survives proxy restart (no genesis re-sweep)
	$(COMPOSE_ENV) ./scripts/e2e-reconciler-cursor-persistence.sh

# --- RD-940 e2e -----------------------------------------------------------
# All RD-940 e2e scripts require the writer worker active. The `e2e-rd940-up`
# helper sets AGGLAYER_ENABLE_WRITER_WORKER=true before bringing up the stack.

.PHONY: e2e-rd940-up
e2e-rd940-up: e2e-clean-data ## Bring up the stack with the RD-940 writer worker enabled
	AGGLAYER_ENABLE_WRITER_WORKER=true \
	  AGGLAYER_WRITER_QUEUE_DEPTH=$${AGGLAYER_WRITER_QUEUE_DEPTH:-64} \
	  AGGLAYER_WRITER_TX_TTL=$${AGGLAYER_WRITER_TX_TTL:-300} \
	  AGGLAYER_CLAIM_RECEIPT_EXPIRATION_BLOCKS=$${AGGLAYER_CLAIM_RECEIPT_EXPIRATION_BLOCKS:-120} \
	  $(E2E_COMPOSE) up -d --build --wait

.PHONY: e2e-rd940-async-submit
e2e-rd940-async-submit: e2e-rd940-up ## Golden async-submit + metric registration
	$(COMPOSE_ENV) ./scripts/e2e-rd940-async-submit.sh

.PHONY: e2e-rd940-pending-receipt
e2e-rd940-pending-receipt: e2e-rd940-up ## Spec D wire-shape: null vs status:0x0 contract
	$(COMPOSE_ENV) ./scripts/e2e-rd940-pending-receipt.sh

.PHONY: e2e-rd940-queue-backpressure
e2e-rd940-queue-backpressure: e2e-rd940-up ## -32005 mapping under concurrent burst
	$(COMPOSE_ENV) ./scripts/e2e-rd940-queue-backpressure.sh

.PHONY: e2e-rd940-restart-inflight
e2e-rd940-restart-inflight: e2e-rd940-up ## SIGTERM -> graceful drain -> dropped_on_restart accounting
	$(COMPOSE_ENV) ./scripts/e2e-rd940-restart-inflight.sh

.PHONY: e2e-rd940-worker-panic
e2e-rd940-worker-panic: e2e-rd940-up ## Failure-metric registration + claim_watcher floor
	$(COMPOSE_ENV) ./scripts/e2e-rd940-worker-panic.sh

.PHONY: e2e-rd940-claim-guard-cancellation
e2e-rd940-claim-guard-cancellation: e2e-rd940-up ## 32 concurrent disconnects, no leaked locks
	$(COMPOSE_ENV) ./scripts/e2e-rd940-claim-guard-cancellation.sh

.PHONY: e2e-rd940
e2e-rd940: e2e-rd940-up ## Run all 6 RD-940 e2e scripts in sequence
	@set -e; \
	for s in async-submit pending-receipt queue-backpressure restart-inflight \
	         worker-panic claim-guard-cancellation; do \
	    echo ""; echo "── RD-940 e2e: $$s ──"; \
	    $(COMPOSE_ENV) ./scripts/e2e-rd940-$$s.sh; \
	done

.PHONY: repro-rd862
repro-rd862: ## Run RD-862 GER-injection race repro (assumes stack is up); prints orphan rate
	N_DEPOSITS=$${N_DEPOSITS:-30} INTER_DELAY_MS=$${INTER_DELAY_MS:-0} POLL_TIMEOUT=$${POLL_TIMEOUT:-300} \
		./scripts/e2e-rd862-repro.sh

.PHONY: test-e2e-coverage
test-e2e-coverage: SHELL := /bin/bash
test-e2e-coverage: ## Regression-protect all three production fixes (RD-862 GER race + claim_watcher synthesis + L2→L1) on a single fresh stack
	@echo "╔══════════════════════════════════════════════════════════════════════════╗"
	@echo "║  test-e2e-coverage: locks production fixes in CI                           ║"
	@echo "║    1) e2e-l1-to-l2                  baseline L1→L2 flow                    ║"
	@echo "║    2) e2e-claim-watcher             watcher happy path (already_recorded)  ║"
	@echo "║    3) e2e-claim-watcher-synthesis   watcher synthesis path (EFAD repro)    ║"
	@echo "║    4) repro-rd862                   orphan-rate canonical metric           ║"
	@echo "║    5) e2e-l2-to-l1-best-effort      L2→L1 round-trip (env-skip aware)      ║"
	@echo "╚══════════════════════════════════════════════════════════════════════════╝"
	@$(MAKE) e2e-down >/dev/null 2>&1 || true
	@set -o pipefail; \
		$(MAKE) e2e-claim-watcher-synthesis; EXIT_CODE=$$?; \
		if [ $$EXIT_CODE -eq 0 ]; then \
			echo ""; \
			echo "Running RD-862 repro on the live stack..."; \
			$(MAKE) repro-rd862; EXIT_CODE=$$?; \
		fi; \
		if [ $$EXIT_CODE -eq 0 ]; then \
			echo ""; \
			echo "Running L2→L1 best-effort (extended timeout + miden-node crash detection)..."; \
			$(COMPOSE_ENV) ./scripts/e2e-l2-to-l1-best-effort.sh; L2L1_EXIT=$$?; \
			case $$L2L1_EXIT in \
				0) echo "  L2→L1: PASS" ;; \
				2) echo "  L2→L1: SKIP (upstream miden-node v0.14.10 instability) — not a regression" ;; \
				*) echo "  L2→L1: FAIL (real regression candidate)"; EXIT_CODE=$$L2L1_EXIT ;; \
			esac; \
		fi; \
		echo ""; \
		echo "Tearing down..."; \
		$(MAKE) e2e-down >/dev/null 2>&1 || true; \
		exit $$EXIT_CODE

.PHONY: e2e
e2e: test-e2e ## Alias for test-e2e (start, test, teardown)

.PHONY: e2e-down
e2e-down: ## Stop E2E environment
	$(E2E_COMPOSE) down -v

.PHONY: e2e-logs
e2e-logs: ## Tail all E2E service logs
	$(E2E_COMPOSE) logs -f
