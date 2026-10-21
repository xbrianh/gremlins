MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)

# sccache writes root-owned files that break subsequent builds; clear it.
# /../cargo-target is not writable by the current user.
# /usr/local/cargo/registry/cache has root-owned .crate files.
RUSTC_WRAPPER :=
CARGO_TARGET_DIR := $(shell pwd)/target
CARGO_HOME := $(shell pwd)/.cargo-home

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
