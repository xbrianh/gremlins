## Development environment

A Python virtual environment is available at `.venv` in the working directory.

## WHen in Doubt

run `make install`. This refreshes the .so files. Do this first if you cannot resolve an error.

## Makefile recipes

Always use `make` targets rather than raw commands. The Makefile handles
parallelism, dependency ordering, and ensures the native extension is built.

| Recipe | What it does | When to use |
|---|---|---|
| `make install` | `maturin develop` — compiles and installs a fresh `.so` in the venv. | **After any Rust change** — this is the one-shot. |
| `make release` | `maturin develop --release` (optimized build). | For production installs. |
| `make check` | Lint + format-check + pyright + rustfmt + clippy. | Before commit. |
| `make test` | `install` + Rust tests + all Python tests. | Before PR / final verify. |
| `make autoformat` | Auto-fix Python and Rust formatting issues. | After messy edits. |

### Building the native extension

`make install` runs `maturin develop` directly, which compiles the crate and
installs the `.so` into the venv in one step. There is no separate `build` or
`dev` target — `maturin develop` handles the full build internally.

### Running tests

**Always run the full suite with `make test`.** It runs Python tests in
parallel (one Make sub-target per `tests/test_*.py` file):

```bash
make test
```

The Makefile auto-detects core count, so explicit `-j` flags are unnecessary
but harmless.

For a single test file:

```bash
make tests/test_active_children.py
```

This triggers `install` (rebuild + install) then runs just that file.

**Never use `uv run pytest`** — the venv is the test target, not whatever
`uv run` resolves.

For a full clean:

```bash
cargo clean -p gremlins-pyext && make install
```

### Check before you think you're done

```bash
make check      # passes fast, but doesn't run tests
make test       # full verification (slower)
```

`make check` does **not** depend on `install`, so if the `.so` is stale it will
pass even when tests would fail. Always run `make test` (or at least
`make install`) after Rust changes.

## Batch replacement over iterative edits

When making the same mechanical change across many files (e.g., fixing a
missing separator in template references), do a single `grep -rn` to
enumerate all occurrences, then apply every edit in one batch pass.
Iterating "find one → fix → find another" wastes turns and risks
introducing regressions from partial fixes.

## Python ↔ Rust boundary

This project has an incremental Rust port. Two crates exist:
- **`crates/gremlins`** — Pure Rust library, no PyO3. Testable standalone
  with `cargo test -p gremlins`.
- **`crates/pyext`** — PyO3 native extension compiling to `_gremlins_core`,
  a Python C extension callable via `import _gremlins_core`.

### What is actively live from Rust

- **`_gremlins_core.utils.proc`** — Process execution (`run`, `run_or_raise`, etc.).
  Re-exported by `gremlins/utils/proc.py`. Most call sites use it.
  A few still use `subprocess` directly: `gremlins/env_file.py`, `gremlins/queue/core.py`,
  `gremlins/utils/spawn_logged_process.py`, and some helpers in `gremlins/utils/proc.py`.
- **`_gremlins_core.clients.RustClient`** — LLM client backend. Wrapped in
  `gremlins/clients/__init__.py`. Handles all provider API calls.
- **`_gremlins_core.config`** — Config accessors (`project_root`, `scratch_root`,
  `state_root`, `overlay_dirname`, `work_root`, etc.).

### How to check if a Rust function is live

```bash
grep -rnE '_gremlins_core\.schemas|from _gremlins_core' gremlins/ --include='*.py'
```

If the Python call site still exists and is referenced, that's the active one.
Delete the Rust function; if nothing breaks, it wasn't wired in.
