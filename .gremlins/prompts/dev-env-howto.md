## Development environment

A pure Rust workspace.

## Makefile recipes

Always use `make` targets rather than raw commands. The Makefile handles
parallelism and ensures consistent flags.

| Recipe | What it does | When to use |
|---|---|---|
| `make build` | `cargo build` (debug build). | Quick compile check. |
| `make release` | `cargo build --release` (optimized build). | Production builds. |
| `make test` | `cargo test -q -p gremlins --lib`. | Before commit / PR. |
| `make check` | `fmt-check` + `clippy` + `cargo check`. | Before commit. |
| `make autoformat` | `cargo fmt --all` + `cargo clippy --fix`. | After messy edits. |
| `make fmt` | `cargo fmt --all`. | Format only. |
| `make fmt-check` | `cargo fmt --all -- --check`. | CI format gate. |
| `make clippy` | `cargo clippy -q --all-targets -- -D warnings`. | Lint gate. |
| `make install` | `cargo install --path crates/gremlins-cli`. | System-wide install. |
| `make test-overlay-tools` | `bats .gremlins/bin/tests/`. | After editing overlay scripts. |

### Running tests

To run a targeted single test:

```bash
cargo test -p gremlins --lib -- <test_name>
```

To run the entire suite:

```bash
make test
```

The Makefile auto-detects core count, so explicit `-j` flags are unnecessary but harmless.

## Batch replacement over iterative edits

When making the same mechanical change across many files (e.g., fixing a
missing separator in template references), do a single `rg` to
enumerate all occurrences, then apply every edit in one batch pass.
Iterating "find one → fix → find another" wastes turns and risks
introducing regressions from partial fixes.
