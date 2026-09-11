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

### `_gremlins_core.clients.Client`

The LLM client backend. There is no Python wrapper — call sites import
`Client` directly from `_gremlins_core.clients`. This handles all provider
API calls.

### `_gremlins_core.discovery.*`

The Rust discovery module (`crates/gremlins/src/core/discovery/mod.rs` +
PyO3 wrapper at `crates/pyext/src/python/discovery.rs`) is **active**.
All Python call sites import `list_pipelines`, `resolve_pipeline_name`,
and `resolve_pipeline_path` from `_gremlins_core.discovery`. The Python
`gremlins/pipeline/discovery.py` has been deleted.

### `_gremlins_core.schemas.*`

All functions and classes in `_gremlins_core.schemas` are exposed at the
Rust layer. All schema functions are now **active** — the Python
`gremlins/pipeline/loader.py` has been deleted and all call sites import
from `_gremlins_core.schemas`.

> **Reading the `Status` column:** **Active** here means *"has >=1 Python
> caller"* (see "How to check whether a Rust function is live" below) — it
> is **not** a guarantee that the binding must stay. Three rows
> (`check_duplicate_producers`, `fill_stage_clients`, `list_bundled_prompts`)
> currently have **0 Python call sites** and are deliberate **KEEPs**
> (documented scaffolding / called from within Rust only). No row in this
> table was deleted by the section-D Python/Rust surface cleanup.

| Rust export | Status |
|---|---|
| `parse_stage` | **Active**. Imported in `gremlins/spawn/child.py` and `gremlins/stages/sequence.py`. |
| `parse_stages` | **Active**. Imported in `gremlins/stages/sequence.py`, `gremlins/stages/loop.py`, `gremlins/stages/parallel.py`. |
| `fill_names` | **Active**. Imported in `gremlins/launcher.py`. |
| `check_duplicate_producers` | **Active**. Imported in `_gremlins_core.schemas.Pipeline.from_yaml`. |
| `expand_pipeline` | **Active**. Imported in `gremlins/launcher.py`. |
| `Pipeline` class | **Active**. `_gremlins_core.schemas.Pipeline` is the canonical Pipeline. |
| `InputSource` / `InputSources` / `Bootstrap` | **Active**. `_gremlins_core.schemas.*` are the canonical implementations. |
| `source_env` / `validate_source_values` / `substitute_bootstrap_vars` | **Active**. `_gremlins_core.schemas.*` are the canonical implementations. |
| `fill_stage_clients` | **Active**. `_gremlins_core.schemas.fill_stage_clients` (public, no underscore). |

## How to check whether a Rust function is live

1. **Search for the Python import.** `grep -rn '_gremlins_core' gremlins/ --include='*.py'` shows what's actually imported from the native extension.
2. **Check the Python call site.** If the Python function still exists and is referenced from launcher.py, pipeline/__init__.py, or other modules, that's the active one.
3. **The litmus test:** delete the Rust function. If nothing breaks, it wasn't wired in yet.

## Traps for the unwary

### The `expand_pipeline` bundling trap (fixed)

The Rust `expand_pipeline` in `crates/pyext/src/schemas/preprocess.rs`
used to take `yaml_path`, optional `project_root`, `bundled_stage_def_dir`,
`bundled_prompt_dir`, and a resolver callback. The dir params have been
removed — prompts and recipes are now embedded at compile time in
`_gremlins_core.assets`. The Python call sites pass only `yaml_path`,
optional `project_root`, and a resolver callback.

### The discovery name resolution trap

The Rust `discovery` module at `_gremlins_core.discovery.*` contains
`list_pipelines`, `resolve_pipeline_name`, and `resolve_pipeline_path`.
These are now **active** — all Python call sites use the Rust versions.
The Python `gremlins/pipeline/discovery.py` has been deleted.

## Key files

| File | Role |
|---|---|
| `gremlins/utils/proc.py` | Re-exports `_gremlins_core.utils.proc.*` — **active** |
| `gremlins/pipeline/discovery.py` | ~~Pure Python `list_pipelines`, `resolve_pipeline_name`, `resolve_pipeline_path` — **active**~~ **deleted** — replaced by `_gremlins_core.discovery.*` |
| `gremlins/pipeline/loader.py` | ~~Pure Python `parse_stage`, `parse_stages`, `fill_names`, `check_duplicate_producers` — **active**~~ **deleted** — replaced by `_gremlins_core.schemas.*` |
| `gremlins/pipeline/__init__.py` | ~~Python `Pipeline` class — **active**~~ **deleted** — replaced by `_gremlins_core.schemas.Pipeline` |
| `gremlins/pipeline/bootstrap.py` | ~~Python `Bootstrap`, `InputSource`, `InputSources` — **active**~~ **deleted** — replaced by `_gremlins_core.schemas.*` |
| `crates/pyext/src/python/discovery.rs` | Rust `list_pipelines`, `resolve_pipeline_name`, `resolve_pipeline_path` (wraps `crates/gremlins/src/core/discovery/mod.rs`) — **active** |
| `crates/gremlins/src/core/discovery/mod.rs` | Rust discovery implementation — **active** |
| `crates/pyext/src/schemas/loader.rs` | Rust `parse_stage`, `parse_stages`, `fill_names`, `check_duplicate_producers` — **active** (replaces deleted `gremlins/pipeline/loader.py`) |
| `crates/pyext/src/schemas/preprocess.rs` | Rust `expand_pipeline` — **active** |
| `crates/gremlins/src/schemas/expand.rs` | Rust `expand_pipeline` implementation — **active** |
| `gremlins/pipeline/preprocess.py` | ~~Pure Python `expand_pipeline` — **active**~~ **deleted** — all expansion is Rust; prompts/recipes embedded at compile time in `_gremlins_core.assets` |
| `crates/pyext/src/python/assets.rs` | `_gremlins_core.assets.load_bundled_prompt`, `list_bundled_prompts` — **active** |
| `crates/gremlins/src/assets/mod.rs` | Compile-time `phf::Map`s for prompts, recipes, pipelines — **active** |
| `crates/pyext/src/lib.rs` | `#[pymodule]` — registers all `_gremlins_core.*` submodules |
