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
- `boss.py` — `boss_main`. Subcommand: `boss`. Not a stage sequencer —
  drives a chain of child gremlins, subprocessing out to
  `python -m gremlins.handoff` and `python -m gremlins.cli {stop,land,rescue}` between
  each one. State lives in `boss_state.json` (schema preserved
  byte-for-byte from the legacy `bossgremlin.py`).

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

- `boss.py` subprocesses out via `_gremlins_cli_cmd` / `_gremlins_cli_env`.
  The env helper sets `PYTHONSAFEPATH=1` and prepends the package's parent
  to `PYTHONPATH` so `python -m gremlins.*` resolves to
  `~/.claude/gremlins/` regardless of cwd (worktree-shadow protection).
- Boss resume is driven by `boss_state.json`, not the runner's stage
  vocabulary. `launcher.resume()` omits `--resume-from` for boss gremlins;
  if a caller still passes the flag directly, `boss_main` logs that it is
  ignoring it.
- `SIGTERM` is trapped to set `_stop_requested` and forward to the current
  child process; the chain checks `check_stop()` between operations.
