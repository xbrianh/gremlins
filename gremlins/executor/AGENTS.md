# `gremlins/executor/`

Internal pipeline execution package. Contains the unified entry point, the
`StageRunner`, and the `State` class.

## Modules

- `state.py` — `State` class: execution context + `state.json` I/O
  (`resolve_session_dir`, `resolve_state_file`, `patch_state`,
  `pipeline_uses_gh`, `read_pr_url`, `validate_gr_id`).
- `run.py` — `run_pipeline`: unified pipeline entry point. Parses argv, loads
  the pipeline YAML, wires clients, and delegates to `Pipeline.run()`.
  Called by `gremlins.run_pipeline` (the subprocess entry point).
- `pipeline.py` — `Pipeline`: sequences stages with `resume_from` support;
  validates stage types against `STAGE_TYPES` from `pipeline/loader.py`.
