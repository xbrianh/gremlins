# `gremlins/orchestrators/`

Per-pipeline orchestrator entry points. Each module owns one CLI subcommand
(see `../cli.py` dispatch) and wires the appropriate stages from
`../stages/` into a sequence.

## Modules

- `local.py` — `local_main` (loads the selected pipeline YAML, default
  `local.yaml`, runs stages), `review_main` (review-code only),
  `address_main` (address-code only). Subcommands: `local`, `review`,
  `address`.
- `gh.py` — `gh_main`. Subcommand: `gh`. Loads the selected pipeline YAML
  (default `gh.yaml`) and runs stages.
- `boss.py` — thin alias that runs the bundled `boss.yaml` through
  `local_main`. Boss-specific behavior now lives in the `chain` stage.

## Conventions

- Each `*_main(argv)` returns an int exit code; the CLI dispatch in
  `../cli.py` calls them with `sys.argv[2:]`.
- `local.py` and `gh.py` build a real `SubprocessClaudeClient()` by default
  and pass it into stages via the `client: ClaudeClient` seam (see parent
  AGENTS.md). Tests inject a `FakeClaudeClient`. Never have an orchestrator
  spawn `claude -p` directly.
- Stage bodies live in `../stages/`. Orchestrators wire them up (resume
  semantics, signal handlers, session-dir resolution) — keep stage logic
  out of these files.
- Stage-name vocabulary is byte-stable (see parent AGENTS.md §"Byte-stable
  strings"). Resume-target validation loads the pipeline YAML and builds
  `all_valid_stages = stage_names + child_names`, where `stage_names` is the
  top-level stage list and `child_names` are stages nested inside parallel
  groups. Both are accepted as `--resume-from` targets.
- `code_style.md` is the canonical coding-style doc for all gremlin pipeline
  stages; edit it there rather than touching `agents/pragmatic-developer.md`.
  It is listed as the first entry in each coding stage's `prompt:` list in
  `local.yaml` / `gh.yaml` and concatenated by `load_prompts(self.prompt_paths)`.
  No special-casing in the orchestrator.

## Boss-specific notes

- The bundled boss flow is a normal local pipeline:
  `chain -> review-chain -> address-chain`.
- `chain` runs the named child pipeline in-process on the boss branch, stores
  `current_child_stage` / `handoff_history` in `state.json`, and re-enters the
  child on resume.
