MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)

# The sandboxed runner ships a pre-populated registry in `.cargo-home` while
# leaving `CARGO_HOME`/`CARGO_TARGET_DIR` pointed at host paths this user
# cannot open. Prefer the repo-local copies when present so plain `cargo`
# invocations work without touching the shared cache.
ifneq ($(wildcard .cargo-home),)
CARGO_HOME := $(abspath .cargo-home)
CARGO_TARGET_DIR := $(abspath target)
endif

export CARGO_HOME
export CARGO_TARGET_DIR

.PHONY: test test-cli check fmt fmt-check clippy build release autoformat test-overlay-tools

# --- Test ---

test:
	cargo test -q -p gremlins --lib

test-cli:
	cargo test -q -p gremlins-cli

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
	cargo clippy -q --all-targets -- -D warnings -A deprecated

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
