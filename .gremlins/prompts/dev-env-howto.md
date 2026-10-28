## Development environment

A pure Rust workspace. No Python, no PyO3, no native extensions.

## When in doubt

run `cargo check`. It's the quickest way to verify compilation and is usually
enough. Don't reach for the full test suite here.

## Avoid repetitive commands

Never re-run the same command unless you've made code changes. An identical
`cargo check` or `cargo test` without intervening edits will produce the same
result and just wastes a turn.

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

A dedicated verify stage runs `make check && make test` after this stage
finishes, so do **not** run the full `make test` suite yourself. For the code
you changed, run a targeted single test:

```bash
cargo test -p gremlins --lib -- <test_name>
```

If you need the full suite, `make test` runs `cargo test -q -p gremlins --lib`,
but leave that to the verify stage. The Makefile auto-detects core count, so
explicit `-j` flags are unnecessary but harmless.

### Check before you think you're done

```bash
make check      # format + clippy + cargo check (no tests)
```

`make check` is fine to run locally — it's format + clippy + cargo check with
no tests. As noted above, leave `make test` to the verify stage. For the code
you changed, `cargo check` is sufficient to verify compilation, and a targeted
`cargo test -p gremlins --lib -- <test_name>` is fine.

## Batch replacement over iterative edits

When making the same mechanical change across many files (e.g., fixing a
missing separator in template references), do a single `rg` to
enumerate all occurrences, then apply every edit in one batch pass.
Iterating "find one → fix → find another" wastes turns and risks
introducing regressions from partial fixes.

## Searching code safely

**Always use `rg` (ripgrep), never `grep -r`.** `rg` respects `.gitignore`
and skips `target/` by default. `grep -r` with an empty file list (e.g.
from a `$(find …)` that returned nothing) silently falls back to searching
the entire working directory — including `target/` — which can dump
megabytes of garbage into context and overflow the model's token window.

When you need to understand a dependency's trait or type definition, look
in the cargo registry source, not the worktree:

```bash
# Find where a dependency's source lives
find ~/.cargo/registry/src -path "*<crate-name>*" -name "*.rs" | head -5
```

Do **not** run `cargo doc` just to grep its output. The generated HTML
is enormous and useless for code search.