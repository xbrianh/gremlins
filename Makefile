PYTHON ?= python

MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc) --output-sync=line

TEST_FILES := $(wildcard tests/test_*.py)

.PHONY: lint format format-write autoformat typecheck test check \
        rust-test rust-fmt rust-fmt-check rust-clippy install release \
        validate-gremlin-overlays test-github-integration-scripts \
        $(TEST_FILES)

lint:
	$(PYTHON) -m ruff check .

format:
	$(PYTHON) -m ruff format --check .

format-write:
	$(PYTHON) -m ruff format .

autoformat: format-write rust-fmt
	$(PYTHON) -m ruff check --fix .
	cargo clippy --fix --all-targets --allow-dirty

typecheck:
	$(PYTHON) -m pyright

test: rust-test $(TEST_FILES)

$(TEST_FILES): install
	$(PYTHON) -m pytest -q --tb=short $@

# --- Rust ---

rust-test: install
	cargo test -q -p gremlins --lib && cargo test -q -p gremlins-pyext --lib

rust-fmt:
	cargo fmt --all

rust-fmt-check:
	cargo fmt --all -- --check

rust-clippy:
	cargo clippy -q --all-targets -- -D warnings

# --- Build ---

install: ## Build and install the native extension
	maturin develop

release: ## Build and install the native extension in release mode
	maturin develop --release

check: lint format typecheck rust-fmt-check rust-clippy
	@grep -r 'from gremlins.executor.state' gremlins/ --include='*.py' | grep -v 'gremlins/executor/' && echo 'ERROR: state.py leak' && exit 1 || true

# --- Shell tests (bats) ---

test-github-integration-scripts:
	bats .gremlins/bin/tests/

# --- Validate ---

GREMLIN_DEFS := $(wildcard .gremlins/*.yaml .gremlins/*.yml)

validate-gremlin-overlays: install $(GREMLIN_DEFS)

.PHONY: $(GREMLIN_DEFS)
$(GREMLIN_DEFS): install
	$(PYTHON) -m gremlins validate $@
