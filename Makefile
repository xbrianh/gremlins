MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc) --output-sync=line

.PHONY: test check fmt fmt-check clippy build release validate-gremlin-definitions autoformat test-overlay-tools

# --- Test ---

test:
	cargo test -q -p gremlins --lib

# --- Check ---

check: fmt-check clippy
	cargo check

# --- Format ---

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

# --- Lint ---

clippy:
	cargo clippy -q --all-targets -- -D warnings

# --- Autoformat ---

autoformat: fmt
	cargo clippy --fix --all-targets --allow-dirty

# --- Build ---

build:
	cargo build

release:
	cargo build --release

validate-gremlin-definitions: build
	@for f in .gremlins/*.yaml; do \
		echo "validate-gremlin-definitions $$f..."; \
		./target/debug/gremlins validate "$$f" || exit 1; \
	done

# --- Overlay Tools ---

test-overlay-tools:
	bats .gremlins/bin/tests/

# --- Install ---

PREFIX ?= /usr/local

install:
	cargo install --path crates/gremlins-cli --root $(DESTDIR)$(PREFIX)
