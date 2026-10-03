MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)

.PHONY: test check fmt fmt-check clippy build release autoformat test-overlay-tools

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

test-overlay-tools:
	bats .gremlins/bin/tests/

# --- Install ---

PREFIX ?= /usr/local

install:
	cargo install --path crates/gremlins-cli --root $(DESTDIR)$(PREFIX)
