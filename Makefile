.DEFAULT_GOAL := help

.PHONY: help
help: ## Show description of all commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# --- Linting -------------------------------------------------------------------------------------

.PHONY: clippy-fix
clippy-fix: ## cargo clippy: fix problems if possible
	cargo clippy --workspace --release --all-targets --quiet --fix --allow-dirty -- -D warnings

.PHONY: clippy
clippy: ## cargo clippy
	cargo clippy --workspace --release --all-targets --quiet -- -D warnings

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
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo doc --lib --no-deps --all-features --keep-going --release

.PHONY: doc-open
doc-open: ## Generate Rust docs and open a browser
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo doc --lib --no-deps --all-features --keep-going --release --open

# --- Testing -------------------------------------------------------------------------------------

.PHONY: test
test: ## Run tests
	cargo nextest run --workspace --release --lib --no-tests pass

.PHONY: test-docs
test-docs: ## Run documentation tests
	cargo test --doc --release

# --- Integration testing -------------------------------------------------------------------------

.PHONY: start-node
start-node: ## Start the test node
	RUST_LOG=info cargo run --release --bin test_node --locked

.PHONY: stop-node
stop-node: ## Stop the test node
	-pkill -f "test_node"
	sleep 1

.PHONY: node
node: start-node ## Start the test node

# --- Building ------------------------------------------------------------------------------------

.PHONY: build
build: ## Build all the binaries
	cargo build --workspace --release

.PHONY: check
check: ## cargo check: compile without producing binaries
	cargo check --workspace --release

.PHONY: fix
fix: ## cargo fix: cargo check and fix warnings if possible
	cargo fix --workspace --release --all-targets --allow-staged --allow-dirty

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
