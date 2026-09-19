MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc) --output-sync=line

.PHONY: test check fmt fmt-check clippy build release

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

# --- Build ---

build:
	cargo build

release:
	cargo build --release