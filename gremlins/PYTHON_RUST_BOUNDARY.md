# Python ↔ Rust boundary

This document describes the current interface between Python and the
`_gremlins_core` native extension (the `crates/pyext` crate) during the
ongoing Rust port. It is a snapshot of the *current* transition state —
everything here will change as more code moves to Rust.

## Why this exists

The codebase has two crates in the workspace:

- **`crates/gremlins`** — Pure Rust library (clients, core utils, discovery,
  schemas). No PyO3 dependency. Can be tested standalone.
- **`crates/pyext`** — PyO3 native extension (`name = "gremlins-pyext"`) that
  compiles to `_gremlins_core`, a Python C extension callable via
  `import _gremlins_core`. It depends on `crates/gremlins` and wraps its
  modules with PyO3 glue.

The port is incremental: a Rust module is built inside `crates/gremlins`,
then wrapped in `crates/pyext` with PyO3 glue, then the Python call site
is swapped from the Python implementation to `import _gremlins_core`.

This means **an exported Rust function may not yet be wired into the
Python execution path**. Looking at a Rust function signature tells you
nothing about whether it's actually being called at runtime. Always check
whether a Python call site has been updated to use it.

## What is actively used from Rust

### `_gremlins_core.utils.proc`

The primary process execution path. Python `gremlins/utils/proc.py` imports from
`_gremlins_core.utils.proc` and re-exports the Rust functions under the
same names (`run`, `run_or_raise`, `run_async`, `run_ok`, etc.).

Most call sites in the codebase go through this module, but a few still use
`subprocess` directly: `gremlins/env_file.py`, `gremlins/queue/core.py`,
`gremlins/utils/spawn_logged_process.py`, and some specialized async helpers
in `gremlins/utils/proc.py` itself.

### `_gremlins_core.clients.RustClient`

The LLM client backend. Python `gremlins/clients/__init__.py` imports
`RustClient` and wraps it. This handles all provider API calls.

## What is exposed but NOT yet called from Python

### `_gremlins_core.discovery.*`

The Rust discovery module (`crates/gremlins/src/core/discovery/mod.rs` + PyO3
wrapper at `crates/pyext/src/python/discovery.rs`) is now **active**.
All Python call sites import `list_pipelines`, `resolve_pipeline_name`, and
`resolve_pipeline_path` from `_gremlins_core.discovery`. The Python
`gremlins/pipeline/discovery.py` has been deleted.

### `_gremlins_core.schemas.*`

All functions and classes in `_gremlins_core.schemas` are exposed at the
Rust layer. `expand_pipeline` is now **active** — all Python call sites
use the Rust implementation. The remaining functions are still Python-only.

| Rust export | Status |
|---|---|
| `parse_stage` | Exposed at `_gremlins_core.schemas.parse_stage` but **not called**. Active implementation: `gremlins/pipeline/loader.py:parse_stage` (pure Python). |
| `parse_stages` | Exposed but **not called**. Active: `gremlins/pipeline/loader.py:parse_stages`. |
| `fill_names` | Exposed but **not called**. Active: `gremlins/pipeline/loader.py:fill_names`. |
| `check_duplicate_producers` | Exposed but **not called**. Active: `gremlins/pipeline/loader.py:check_duplicate_producers`. |
| `expand_pipeline` | Exposed at `_gremlins_core.schemas.expand_pipeline` — **active**. All Python call sites use the Rust implementation. |
| `Pipeline` class | Exposed at `_gremlins_core.schemas.Pipeline` but **not used**. Active: `gremlins/pipeline/__init__.py:Pipeline`. |
| `InputSource` / `InputSources` | Exposed but **not called** from Python bootstrap code. |

## How to check whether a Rust function is live

1. **Search for the Python import.** `grep -rn '_gremlins_core' gremlins/ --include='*.py'` shows what's actually imported from the native extension.
2. **Check the Python call site.** If the Python function still exists and is referenced from launcher.py, pipeline/__init__.py, or other modules, that's the active one.
3. **The litmus test:** delete the Rust function. If nothing breaks, it wasn't wired in yet.

## Traps for the unwary

### The `expand_pipeline` bundling trap

The Rust `expand_pipeline` in `crates/pyext/src/schemas/preprocess.rs`
takes `bundled_prompt_dir`, `bundled_stage_def_dir`, and
`bundled_pipeline_dir` as explicit `PathBuf` parameters. The Python call
sites pass `BUNDLED_PROMPT_DIR`, `BUNDLED_STAGE_DEF_DIR`, and a resolver
callback to the Rust function at each call site.

### The discovery name resolution trap

The Rust `discovery` module at `_gremlins_core.discovery.*` contains
`list_pipelines`, `resolve_pipeline_name`, and `resolve_pipeline_path`.
These are now **active** — all Python call sites use the Rust versions.
The Python `gremlins/pipeline/discovery.py` has been deleted.

## Key files

| File | Role |
|---|---|
| `gremlins/_core.py` | Shim: `import _gremlins_core as _core; __all__ = ["_core"]` |
| `gremlins/utils/proc.py` | Re-exports `_gremlins_core.utils.proc.*` — **active** |
| `gremlins/clients/__init__.py` | Wraps `_gremlins_core.clients.RustClient` — **active** |
| `gremlins/pipeline/discovery.py` | ~~Pure Python `list_pipelines`, `resolve_pipeline_name`, `resolve_pipeline_path` — **active**~~ **deleted** — replaced by `_gremlins_core.discovery.*` |
| `gremlins/pipeline/loader.py` | Pure Python `parse_stage`, `parse_stages`, `fill_names`, `check_duplicate_producers` — **active** (Rust equivalents at `_gremlins_core.schemas.*` exist but are **not wired** into any Python call site) |
| `crates/pyext/src/python/discovery.rs` | Rust `list_pipelines`, `resolve_pipeline_name`, `resolve_pipeline_path` (wraps `crates/gremlins/src/core/discovery/mod.rs`) — **active** |
| `crates/gremlins/src/core/discovery/mod.rs` | Rust discovery implementation — **active** |
| `crates/pyext/src/schemas/loader.rs` | Rust `parse_stage`, `parse_stages`, `fill_names`, `check_duplicate_producers` — **NOT yet active** (parallel implementations; Python originals in `gremlins/pipeline/loader.py` are the active ones) |
| `crates/pyext/src/schemas/preprocess.rs` | Rust `expand_pipeline` — **active** |
| `crates/pyext/src/lib.rs` | `#[pymodule]` — registers all `_gremlins_core.*` submodules |
