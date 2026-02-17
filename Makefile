.DEFAULT_GOAL := help

CARGO_PROFILE ?= dev
CARGO_RELEASE_ARG := $(if $(filter release,$(CARGO_PROFILE)),--release,)

.PHONY: help
help: ## Show description of all commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

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
test: ## Run tests
	cargo nextest run --workspace $(CARGO_RELEASE_ARG) --lib --no-tests pass

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
