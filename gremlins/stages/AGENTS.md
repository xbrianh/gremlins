# `gremlins/stages/`

Per-stage bodies — the work each pipeline stage actually does. Modules
here are called by orchestrators in `../orchestrators/`; they own no
sequencing logic of their own.

## Rust-ported stages

- `agent` (type `"agent"`) — The generic agentic primitive. Now a Rust
  `PyAgent` class in `_gremlins_core.stages.Agent`. Declared in YAML with
  `interpolation:` / `bind:` maps. Resolves interpolation artifacts,
  registers bind URIs, builds a prompt with workspace preamble, delegates
  to a model client, records token usage, checks for bail, and verifies
  single-output artifacts.
  - Prompt template is `"\n\n".join(prompts)`; `{var}` tokens are substituted
    via interpolation_map → bind_paths → framework_subs resolution order.
  - `state.artifacts` is required only when `interpolation:` or `bind:` maps
    are non-empty; stages with empty maps can delegate to `Agent` without
    a registry.
- `exec` — `Exec(Stage)`. Runs `options["cmds"]` joined with `&&`;
  writes combined stdout+stderr to `exec-{name}.log`. On non-zero exit:
  raises `Bail` unless exit code 2 with `bail` in the `out:` map (which sets
  the bail artifact). Supports `interpolation:`/`bind:` artifact substitution
  and `git://range` out-URIs. Stage type `"exec"`.
  Rust `PyExec` class in `_gremlins_core.stages.Exec`.

## Python stages

- `loop.py` — `LoopStage(Stage)`. Iterates a `body: list[Stage]` (or raw
  `body_runners` callables) until `stop_when_exists` artifact is bound or
  `max_iterations` is exhausted. Body stages execute in order on every
  iteration. After each full body run: if a bail artifact is set, raises
  `Bail`; if the `stop_when_exists` artifact is bound, returns `Done()`;
  if `max_iterations` is reached without stopping, raises `Bail`. The
  stopping condition is declared explicitly in the pipeline YAML via
  `stop_when_exists: <artifact-key>`. No magic `status=needs_fix` marker
  or `head_stable` predicate.
- `parallel.py` — `ParallelStage(Stage)`. Constructed by the orchestrator
  with pre-built child runners; call `build_runtime_stages()` to get the
  three `(name, fn)` pairs (`<group>-fanout`, `<group>`, `<group>-fanin`)
  that implement fan-out/fan-in execution.
- `sequence` — `Sequence` (Rust, `_gremlins_core.stages`). Runs
  `body: list[Stage]` sequentially in order, inheriting parent state (no
  fan-out). Child stages share artifacts and execution scope with the
  parent; client override is applied if the child declares one. Useful for
  bundling multi-stage units (e.g., the `handoff` recipe) that should
  appear as a single iteration in a parent loop. Stage type `"sequence"`.
- `composite.py` — Shared helpers for composite stages (`Loop`, `Sequence`,
  `Parallel`): `child_state(parent, child, fan_out=False, child_id=None)`
  derives child state from parent, handling client override and optional
  artifact isolation.

## Recipes

Bundled stage recipes live under `gremlins/recipes/stages/`. Each recipe is a multi-primitive YAML pipeline fragment that expands in-place wherever its `gremlins:<name>` type is referenced.

- `plan` — local planning: `plan` (agent) → `set-description`. Agent output: `plan: file://session/plan.md`. The agent stage has `skip_if_exists: plan`, so a resumed run skips the LLM if a non-empty `plan.md` already exists. The bootstrap block is the canonical source of `--plan`/`--instructions`; when `--plan` references a GitHub issue (`#N`), bootstrap downloads the issue body directly to `plan.md` and saves the issue number as `plan-source-issue-number`.
- `plan_gh` — GitHub planning: `plan` (agent) → `publish-as-issue` → `set-description`. The agent writes `plan: file://session/plan.md` and has `skip_if_exists: plan`; `publish-as-issue` has `skip_if_exists: plan-issue-number` (idempotency guard against duplicate issues on resume). Same `bootstrap.source` requirement as `plan`.
- `handoff` — boss-loop chain manager: `handoff-init` (exec) → `handoff` (agent) → `translate-signal` (exec, routes `signal.json` exit_state to loop primitives: `next-plan`→exit 0 (no done), `chain-done`→write done file, `bail`→exit 2 with bail artifact) → `sanitize` (haiku agent) → `restore-rolling-plan` (exec).
- `implement` — implementation + progress guard.
- `verify` — run commands, fix loop, bail on exhaustion.
- `review-code` — code review agent, writes `{name}-{model}.md`.
- `github-open-pr` — compose PR title/body, push branch, open PR.
- `github-push-to-pr-branch` — push HEAD to existing PR branch.
- `github-request-copilot-review` — add Copilot as PR reviewer.
- `github-wait-copilot` — poll until Copilot posts a non-pending review.
- `github-wait-ci` — poll CI checks, fix loop, bail on `REVIEW_REQUIRED` or exhaustion.

## Conventions

- YAML stage entries are dispatched via `STAGE_TYPES` in `gremlins/pipeline/loader.py`. Each type string maps to a `Stage` subclass; `parse_stage` calls `StageCls.with_dict(d)` to construct the instance and the executor calls `stage.run(state)` to execute it.
- Every stage that talks to a model takes `client: Client` from
  `gremlins.clients.client` and calls `client.run(...)`. **Never spawn
  a model subprocess directly** — that bypasses the test seam.
- Prompt-based stages join `self.prompts` (already-loaded text list) with `"\n\n"`.
  Bundled internal prompts are loaded via `load_bundled_prompt` / `render_bundled_prompt`
  from `gremlins.utils.yaml_io`. Bundled prompt files live under `gremlins/prompts/`. See
  `gremlins/prompts/README.md` for the runtime placeholder inventory.
- Stages that should respect a bail marker delegate to the Rust agent's
  `check_bail`, which parses the agent's final transcript message for a
  `BAIL: <class>: <detail>` sentinel line and raises `Bail` if found.
- Most stages return `None`.
- The `label=` argument passed to `client.run(...)` is the stream-event
  prefix and the `FakeClient` fixture key. Stages that re-enter the
  same logical step within one process (e.g. resumed implement) must use
  distinct labels per phase so the fake's lookup doesn't collide.

## Import constraint for new stages

Any new `gremlins/stages/introspect.py` (planned for #258) must import only `inspect` and `gremlins.stages.composite.StageAttrs` — never any orchestrator module. This keeps the stages package free of upward dependencies so orchestrators can import stages without cycles.

## Load-bearing invariants

- The empty-implementation invariant is enforced by the `require-impl-progress`
  exec stage in the `implement` stage-definition (gh.yaml). It runs two shell
  checks: HEAD must be a fast-forward from `base_sha`, and at least one commit
  must exist since `base_sha`. Either failure raises `Bail`. This is the
  firewall that keeps no-op runs out of the review stage. Don't soften it.
