# `gremlins/orchestrators/`

Per-pipeline orchestrator entry points. Each module owns one CLI subcommand
(see `../cli.py` dispatch) and wires the appropriate stages from
`../stages/` into a sequence.

## Modules

- `local.py` — `local_main` (full local chain: `plan → implement → review-code →
  address-code → test`), `review_main` (review-code only), `address_main`
  (address-code only). Subcommands: `local`, `review`, `address`. The `test`
  stage is a no-op when `--test` is omitted.
- `gh.py` — `gh_main`. Subcommand: `gh`. Drives the gh pipeline:
  `plan → implement → commit-pr → request-copilot → ghreview →
  wait-copilot → ghaddress`. The `request-copilot` stage body is inlined
  here as a closure rather than living in `../stages/`.
- `boss.py` — `boss_main`. Subcommand: `boss`. Not a stage sequencer —
  drives a chain of child gremlins, subprocessing out to
  `python -m gremlins.cli {handoff,fleet}` between each one. State lives in
  `boss_state.json` (schema preserved byte-for-byte from the legacy
  `bossgremlin.py`).

## Conventions

- Each `*_main(argv)` returns an int exit code; the CLI dispatch in
  `../cli.py` calls them with `sys.argv[2:]`.
- `local.py` and `gh.py` build a real `SubprocessClaudeClient()` by default
  and pass it into stages via the `client: ClaudeClient` seam (see parent
  CLAUDE.md). Tests inject a `FakeClaudeClient`. Never have an orchestrator
  spawn `claude -p` directly.
- Stage bodies live in `../stages/`. Orchestrators wire them up (resume
  semantics, signal handlers, session-dir resolution) — keep stage logic
  out of these files.
- Stage-name vocabulary per orchestrator is byte-stable (see parent
  CLAUDE.md §"Byte-stable strings"). `VALID_RESUME_STAGES` /
  `VALID_STAGES` constants are the source of truth.
- Both `local.py` and `gh.py` call `load_code_style()` from
  `gremlins.prompts`, which reads `gremlins/prompts/code_style.md` via a
  `pathlib.Path(__file__)` resolve in `prompts/__init__.py`. This is the
  canonical coding-style doc for all gremlin pipeline stages; edit it there
  rather than touching `agents/pragmatic-developer.md`.

## Boss-specific notes

- `boss.py` subprocesses out via `_gremlins_cli_cmd` / `_gremlins_cli_env`.
  The env helper sets `PYTHONSAFEPATH=1` and prepends the package's parent
  to `PYTHONPATH` so `python -m gremlins.cli` resolves to
  `~/.claude/gremlins/` regardless of cwd (worktree-shadow protection).
- The `--resume-from` flag forwarded by `launch.sh --resume` is *ignored*:
  boss resumes from `boss_state.json`, not the runner's stage vocabulary.
- `SIGTERM` is trapped to set `_stop_requested` and forward to the current
  child process; the chain checks `check_stop()` between operations.
