MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc) --output-sync=line

.PHONY: test check fmt fmt-check clippy build release validate autoformat

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

# --- Validate ---

validate: build
	@for f in .gremlins/*.yaml; do \
		echo "validate $$f..."; \
		./target/debug/gremlins validate "$$f" || exit 1; \
	done

# --- Install ---

PREFIX ?= /usr/local

install:
	cargo install --path crates/gremlins-cli --root $(DESTDIR)$(PREFIX)