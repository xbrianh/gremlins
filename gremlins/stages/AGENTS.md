# `gremlins/stages/`

Per-stage bodies — the work each pipeline stage actually does. Modules
here are called by orchestrators in `../orchestrators/`; they own no
sequencing logic of their own.

## Modules

- `registry.py` — `STAGE_REGISTRY` and `CLIENT_FACTORIES` dicts + `register_stage` / `register_client_factory`. All stage type lookups go through here.
- `all.py` — importing triggers side-effect registration of all stage modules. `pipeline.py` imports it automatically via `_ensure_registered()`; no manual import needed.
- `context.py` — `StageContext(client, session_dir, gr_id)` dataclass passed as the first arg to every registry-dispatched stage function.
- `plan.py` — `run(ctx, PlanOptions)`. Local pipeline only.
- `implement.py` — `run(ctx, ImplementOptions)`. Dual-mode (`kind='local'` /
  `kind='gh'`). For gh: enforces the empty-implementation invariant,
  classifies the outcome (`HeadAdvanced` / `DirtyOnly` / `EmptyImpl` /
  `DivergentHead`), creates the impl-handoff branch, and returns an
  `ImplStageResult` with the pre-impl state and classified outcome.
- `review_code.py` — `run(ctx, ReviewCodeOptions)`. Local pipeline only
  (single-detail-reviewer post-collapse).
- `address_code.py` — `run(ctx, AddressCodeOptions)`. Local pipeline only.
- `commit_pr.py` — `run(ctx, CommitPrOptions)`. Gh pipeline. Opens a fresh
  claude session against the impl-handoff branch diff; no session_id
  dependency, so `--resume-from commit-pr` works cleanly.
- `ghreview.py` — `run(ctx, GhreviewOptions)`. Thin wrapper around `/ghreview
  <pr_url>` plus a `check_bail` call.
- `ghaddress.py` — `run(ctx, GhaddressOptions)`. Thin wrapper around `/ghaddress
  <pr_url>`.
- `request_copilot.py` — `run`. Requests Copilot review by adding
  `copilot-pull-request-reviewer` to the PR's reviewer list.
- `verify.py` — `run(ctx, VerifyOptions)`. Runs `check_cmd && test_cmd` (skipping empties), re-invokes agent to fix failures up to `max_attempts`; bails on exhaustion. Takes `is_git` (controls diff capture) and `commit_after_fix` (controls commit instruction in fix prompt). Used by both gh and local pipelines. Registers as stage type `"verify"`.
- `wait_ci.py` — `run(ctx, WaitCiOptions)`. Gh pipeline (`ci-gate`). Polls PR CI checks via `gh_utils`; re-invokes agent to fix failures; bails on `REVIEW_REQUIRED` or attempt exhaustion. Registers as stage type `"wait-ci"`.
- `wait_copilot.py` — `run`. Polls until Copilot posts a non-PENDING review.

## Conventions

- Stages registered via `register_stage` receive `(ctx: StageContext, options: XxxOptions)` as positional args.
- Stage modules expose `run(ctx: StageContext, options: XxxOptions)` as the orchestrator entry point. Orchestrators call `module.run(ctx, options)` directly.
- Every stage that talks to `claude` takes `client: ClaudeClient` and
  calls `client.run(...)`. **Never spawn `claude -p` directly** — that
  bypasses the test seam in `../clients/protocol.py`.
- Main stage prompts come from `StageEntry.prompt_paths`, resolved by the
  pipeline loader and threaded through `options` (or passed directly by the
  orchestrator). In pipeline runs, stages receive their primary prompt path
  this way and should not construct it themselves. Stage-local fallback
  constants (e.g. `PROMPT_LOCAL_PATH`, `_DEFAULT_PROMPT`) exist only for
  standalone entry points and backward-compatibility.
- Fix-loop templates (`verify_fix.md`, `ci_fix.md`) are
  intrinsic to their stage module and are resolved via:
  ```python
  pathlib.Path(__file__).resolve().parent / "<name>.md"
  ```
  `__file__`-relative resolution is required because the package installs
  to `~/.claude/gremlins/`; `__file__` is the only cwd-independent anchor.
- Stages that should respect a bail marker (set by the agent via
  `python -m gremlins.bail`) call `check_bail(<phase-name>)` from
  `..state` after the claude run. The runner inspects the bail and
  halts the pipeline.
- Most stages return `None`. Stages that produce information the
  orchestrator needs (`implement.py` → `ImplStageResult`,
  `commit_pr.py` → PR URL string) return it; the orchestrator threads
  it into later stages.
- The `label=` argument passed to `client.run(...)` is the stream-event
  prefix and the `FakeClaudeClient` fixture key. Stages that re-enter the
  same logical step within one process (e.g. resumed implement) must use
  distinct labels per phase so the fake's lookup doesn't collide.

## Load-bearing invariants

- `implement.py` enforces the empty-implementation invariant: an empty
  impl in the gh pipeline raises `EmptyImpl` and the runner bails. This
  is the firewall that keeps no-op runs out of `commit-pr` / `ghreview`.
  Don't soften it.
- `commit_pr.py` selects its action clause based on the `ImplOutcome`
  classification from the implement stage. The three shapes
  (`HeadAdvanced`, `DirtyOnly`, plus the empty-handoff fallback) are
  distinct prompts in `../prompts/` — keep them aligned.
