.DEFAULT_GOAL := help

.PHONY: help
help: ## Show description of all commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# --- Linting -------------------------------------------------------------------------------------

.PHONY: clippy
clippy: ## Run Clippy with configs. We need two separate commands because the `testing-remote-prover` cannot be built along with the rest of the workspace. This is because they use different versions of the `miden-tx` crate which aren't compatible with each other.
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: fix
fix: ## Run Fix with configs, building tests with proper features to avoid type split.
	cargo fix --workspace --all-targets --allow-staged --allow-dirty

.PHONY: format
format: ## Run format using nightly toolchain
	cargo fmt --all

.PHONY: format-check
format-check: ## Run format using nightly toolchain but only in check mode
	cargo fmt --all --check

.PHONY: toml
toml: ## Runs Format for all TOML files
	taplo fmt

.PHONY: toml-check
toml-check: ## Runs Format for all TOML files but only in check mode
	taplo fmt --check --verbose

.PHONY: typos-check
typos-check: ## Run typos to check for spelling mistakes
	@typos --config ./.typos.toml

.PHONY: lint
lint: format fix toml clippy typos-check ## Run all linting tasks at once (clippy, fixing, formatting, typos)

# --- Documentation -------------------------------------------------------------------------------

.PHONY: doc
doc: ## Generate & check rust documentation. Ensure you have the nightly toolchain installed.
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo doc --lib --no-deps --all-features --keep-going --release

.PHONY: doc-open
doc-open: ## Generate & open rust documentation in browser. Ensure you have the nightly toolchain installed.
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo doc --lib --no-deps --all-features --keep-going --release --open

# --- Testing -------------------------------------------------------------------------------------

.PHONY: test
test: ## Run tests
	cargo nextest run --workspace --release --lib

.PHONY: test-docs
test-docs: ## Run documentation tests
	cargo test --doc

# --- Integration testing -------------------------------------------------------------------------

.PHONY: start-node
start-node: ## Start the testing node server
	RUST_LOG=info cargo run --release --bin test_node --locked

.PHONY: stop-node
stop-node: ## Stop the testing node server
	-pkill -f "test_node"
	sleep 1

# --- Building ------------------------------------------------------------------------------------

.PHONY: build
build: ## Build the CLI binary, client library and tests binary in release mode
	CODEGEN=1 cargo build --workspace --release

.PHONY: check
check: ## Build the CLI binary and client library in release mode
	cargo check --workspace --release

## --- Setup --------------------------------------------------------------------------------------

.PHONY: check-tools
check-tools: ## Checks if development tools are installed
	@echo "Checking development tools..."
	@command -v mdbook        >/dev/null 2>&1 && echo "[OK] mdbook is installed"        || echo "[MISSING] mdbook       (make install-tools)"
	@command -v typos         >/dev/null 2>&1 && echo "[OK] typos is installed"         || echo "[MISSING] typos        (make install-tools)"
	@command -v cargo nextest >/dev/null 2>&1 && echo "[OK] cargo-nextest is installed" || echo "[MISSING] cargo-nextest(make install-tools)"
	@command -v taplo         >/dev/null 2>&1 && echo "[OK] taplo is installed"         || echo "[MISSING] taplo        (make install-tools)"

.PHONY: install-tools
install-tools: ## Installs Rust + Node tools required by the Makefile
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
