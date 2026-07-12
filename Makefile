MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)

TEST_FILES := $(wildcard tests/test_*.py)
CRATE_DIR := crates/gremlins-core

.PHONY: lint format format-write typecheck test check \
        rust-test rust-fmt rust-fmt-check rust-clippy dev install \
        $(TEST_FILES)

lint:
	ruff check .

format:
	ruff format --check .

format-write:
	ruff format .

typecheck:
	PYTHONPATH='' pyright --pythonpath $(shell which python)

test: rust-test $(TEST_FILES)

$(TEST_FILES):
	PYTHONPATH='' python -m pytest $@ || { code=$$?; [ $$code -eq 5 ] && exit 0 || exit $$code; }

# --- Rust ---

rust-test:
	cargo test -p gremlins-core --lib

rust-fmt:
	cargo fmt --all

rust-fmt-check:
	cargo fmt --all -- --check

rust-clippy:
	cargo clippy --all-targets -- -D warnings

# --- Build ---

dev: ## Build and install the native extension in dev mode
	cd $(CRATE_DIR) && maturin develop

install: ## Build and install the native extension in release mode
	cd $(CRATE_DIR) && maturin develop --release

check: lint format typecheck rust-fmt-check rust-clippy
	@grep -r 'from gremlins.executor.state' gremlins/ --include='*.py' | grep -v 'gremlins/executor/' && echo 'ERROR: state.py leak' && exit 1 || true
