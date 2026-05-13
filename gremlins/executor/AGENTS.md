# `gremlins/executor/`

Internal pipeline execution package.

## Modules

- `state.py` — `State` class: execution context + `state.json` I/O
  (`resolve_session_dir`, `resolve_state_file`, `patch_state`,
  `read_pr_url`, `validate_gremlin_id`).
- `run.py` — `run_pipeline`: unified pipeline entry point. Parses argv, loads
  the pipeline YAML, wires clients, and delegates to `Gremlin`.
  Called by `gremlins.run_pipeline` (the subprocess entry point).
- `gremlin.py` — `Gremlin`: constructs, initializes, and runs a pipeline.
  Sequences stages with `resume_from` support; validates stage types against
  `STAGE_TYPES` from `pipeline/loader.py`.

## Layering

The launcher (`gremlins/launcher.py`) does the minimum work required to fork a
detached child: picks a `gremlin_id`, derives the state-dir path, writes a
preliminary `state.json`, and redirects logging. It does **not** create the
git worktree.

The child process calls `Gremlin(...).initialize_runtime()` then `.run()`.
`initialize_runtime()` owns all side-effectful setup: creating the worktree,
copying `spec.md` / `plan.md`, and `os.chdir` into the worktree so stages
run in the right directory. This is the single code path for both the CLI
(`launcher` → `run_pipeline` → `Gremlin`) and future SDK callers.
