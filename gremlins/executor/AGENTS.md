# `gremlins/executor/`

Internal pipeline execution package. Contains the unified entry point, the
`StageRunner`, the `State` class, and the review/address library helpers.

## Modules

- `state.py` — `State` class: execution context + `state.json` I/O
  (`resolve_session_dir`, `resolve_state_file`, `patch_state`,
  `pipeline_uses_gh`, `read_pr_url`, `validate_gr_id`).
- `run.py` — `run_pipeline`: unified pipeline entry point. Parses argv, loads
  the pipeline YAML, wires clients, and delegates to `Pipeline.run()`.
  Called by `gremlins.run_pipeline` (the subprocess entry point).
- `pipeline.py` — `Pipeline`: sequences stages with `resume_from` support;
  resolves stage objects from `STAGE_REGISTRY` and client specs.
- `review_address.py` — `run_review`, `run_address`: library helpers called by
  `cli/review_address.py` for the standalone `gremlins review` /
  `gremlins address` subcommands.
