# gremlins

Background coding-agent pipelines that plan, implement, review, and land work
end-to-end. Given a goal or GitHub issue, a gremlin runs the full
plan → implement → review-code → address-code cycle unattended, writing
artifacts to the per-user state directory resolved by
`platformdirs.user_state_dir("gremlins")` and optionally opening a pull
request. A fleet manager tracks running, stalled, and finished gremlins and
provides stop / land / close operations.

**Status: brand-new and a bit janky.** This is a fresh project, actively
shaped by daily use. Expect rough edges — stream timeouts, the occasional
merge conflict from parallel gremlins, a few stages still finding their
final shape. Bug reports, ideas, and PRs are all welcome.

---

## Using gremlins with a coding assistant

Paste the output of `gremlins prompt-for-assistant` into a fresh Claude Code session (or any compatible assistant) to configure it as a competent gremlins collaborator.

The workflow: you discuss the work with the assistant, it captures discrete units as GitHub issues or plan files, launches gremlins in the background to implement them, and lands each finished gremlin before starting dependent work. You stay at the strategic level — deciding what to build and in what order — while gremlins handle the implementation cycle unattended. The assistant maintains a queue of running, pending, and blocked work and surfaces it on request.

---

## Using gremlins across multiple repos

When you run `gremlins launch`, the launcher captures the current working
directory's repo root via `git rev-parse --show-toplevel` and stores it as
`project_root` in the gremlin's `state.json`. That value pins the worktree
base, child process cwd, and pipeline discovery for that gremlin's lifetime.

**To work on a different repo: `cd` there, then `gremlins launch`.** There is
no `--project-root` flag; the cwd at launch time is the contract.

**Fleet view** (`gremlins`) shows gremlins from all repos by default.
Pass `--here` to filter to the current repo's `project_root`.

**Pipeline discovery** walks from the launching cwd, so `.gremlins/pipelines/`
overrides in each repo apply to gremlins launched from that repo.

**Queue caveat**: there is one global queue and the runner's cwd is frozen at
`gremlins queue run --detach` time. To queue work against a different repo,
prefix the command with `cd`:

```sh
gremlins queue add "cd /path/to/other-repo && gremlins launch gh --plan '#42' --wait"
gremlins queue add "cd /path/to/other-repo && gremlins land <id>"
```

**State isolation**: each gremlin's state lives under its own directory
(resolved via `platformdirs.user_state_dir("gremlins")/<id>/`), so two repos
can have running gremlins simultaneously without interference.

---

## Runtime CLI prerequisites

- `gh` — [GitHub CLI](https://github.com/cli/cli#installation)
- `git` — [Git](https://git-scm.com/downloads) (pre-installed on most systems)

A provider also requires either its API key (`OPENAI_API_KEY`, `XAI_API_KEY`,
`OPENROUTER_API_KEY`) or a `cmd:` command on `PATH`.

## Dev install

```sh
uv venv
source .venv/bin/activate  # or `.venv\Scripts\activate` on Windows
uv pip install -e ".[dev]"
make install               # build + install the Rust native extension
```

Run `uv pip install -e ".[dev]"` **before** `make install` — the dev extra installs `maturin`, which `make install` requires.

## Make targets

| Target | What it runs |
|---|---|
| `make install` | `maturin develop` |
| `make release` | `maturin develop --release` |
| `make test` | `cargo test -p gremlins --lib && cargo test -p gremlins-pyext --lib`, then each `tests/test_*.py` via pytest |
| `make lint` | `ruff check .` |
| `make format` | `ruff format --check .` |
| `make format-write` | `ruff format .` |
| `make typecheck` | `pyright` (gremlins/) |
| `make rust-test` | `cargo test -p gremlins --lib && cargo test -p gremlins-pyext --lib` |
| `make rust-fmt` | `cargo fmt --all` |
| `make rust-fmt-check` | `cargo fmt --all -- --check` |
| `make rust-clippy` | `cargo clippy --all-targets -- -D warnings` |
| `make check` | lint + format + typecheck + rust-fmt-check + rust-clippy |

## CLI subcommands

Invoked as `python -m gremlins.cli <subcommand>` or `gremlins <subcommand>`
after install. The authoritative list and per-subcommand description lives in
the dispatch table in [`gremlins/cli/__init__.py`](gremlins/cli/__init__.py).

| Subcommand | Purpose |
|---|---|
| `launch <name>` | Launch a background gremlin by pipeline name (`gremlins launch --list` to see available) |
| `resume` | Re-spawn an existing gremlin from its recorded stage |
| `stop` | Send SIGTERM to a running gremlin and wait for it to exit |
| `land` | Land a finished gremlin onto the current branch, then clean up |
| `rm` | Delete a gremlin's state directory, worktree, and branch |
| `close` | Mark a gremlin as closed (hidden from default view) |
| `log` | Tail the gremlin's log file |
| `ack` | Acknowledge a gremlin waiting for human input |
| `skip` | Skip a gremlin waiting for human input |
| `queue` | Manage the gremlin launch queue |
| `prompt-for-assistant` | Print the assistant setup prompt to stdout |
| `artifacts` | Inspect artifact keys and bindings |
| `clean` | Clean finished gremlin state directories |

### `queue` sub-subcommands

| Sub-subcommand | Description |
|---|---|
| `add [--run] <command>` | Add a command to the queue; `--run` also starts the runner if idle |
| `list [--watch] [--json]` | List queued items |
| `run [--once] [--poll-interval SEC] [--detach]` | Start the queue runner |
| `requeue [--done]` | Move failed (and optionally done) items back to pending |
| `clear [--failed\|--done\|--pending\|--purge\|--item STEM]` | Remove items from the queue |
| `set-state <state> --item STEM` | Manually transition a queue item to a different state |
| `stop` | Stop the detached runner |

### Launch flags

#### Universal flags

These flags are accepted by every pipeline:

| Flag | Description |
|---|---|
| `--description <text>` | Human-readable description stored in state |
| `--gremlin-id <id>` | Use a specific gremlin id (must match `[A-Za-z0-9_-]+`) |
| `--parent <id>` | Parent gremlin ID (used by boss to track child ownership) |
| `--print-id` | Print the gremlin ID to stdout after launch |
| `--print-id-only` | Print only the gremlin id on stdout; suppress the launch banner |
| `--wait` | Block until the spawned gremlin exits; return its exit code |
| `--base-ref <ref>` | Git ref to branch the worktree from; defaults to the pipeline's `base_ref` (which defaults to `"current"`) |
| `--client <spec>` | `provider:model` string overriding the pipeline's `default_client` |
| `--telemetry` / `-v` | Enable per-turn telemetry (TTFT, token counts, cache hit ratio) in the gremlin log |

#### Per-pipeline flags

Additional flags are generated from the pipeline's `bootstrap.source` block.
Each source key becomes a `--<key>` flag (required unless `optional: true`).
For example, the `gh` pipeline declares `plan` and `instructions` sources, so
`gremlins launch gh --help` shows `--plan` and `--instructions`. Run
`gremlins launch <name> --help` to see the full list for a given pipeline.

## Pipeline configuration

Gremlins runs a sequence of stages defined in a YAML file. The bundled
pipelines work out of the box; a project-local YAML can override any of them.

### Discovery order

`--pipeline <name|path>` resolves as follows:

1. A value with a `.yaml` suffix or more than one path component is loaded
   directly as a filesystem path.
2. Otherwise `./.gremlins/pipelines/<name>.yaml` is checked first
   (project-local override).
3. Then `gremlins/pipelines/<name>.yaml` (bundled) is checked.

The pipeline name is the first non-flag argument to `gremlins launch`. Run `gremlins launch --list` to see all available pipeline names.

### Selecting a pipeline

```sh
gremlins launch local   # bundled local.yaml
gremlins launch gh      # bundled gh.yaml
```

### Schema reference

**Top-level keys:**

```yaml
default_client: xai:grok-4    # required; provider:model string

base_ref: current             # optional; git ref to branch worktrees from (default "current")

github_integration: true      # optional; enables gh CLI integration

bootstrap:                    # optional; CLI contract and setup commands
  source:
    plan:
      type: [filepath, string]
      optional: true
  launch_cmds:
    - gremlins:bind_artifact(plan, plan, file://session/plan.md)
  cmds:
    - "uv sync"
  cli_out:
    pr: "opaque://pr/{read:pr-num}"

prompts:                      # optional; named prompt map
  code-style: gremlins:code_style.md

prompt_dir: ../prompts        # optional; relative to YAML, defaults to the YAML's directory

stage-definitions:            # optional; reusable stage patterns
  normalize:
    type: exec
    options:
      cmds: ["ruff format . && ruff check --fix ."]

land:                         # optional; exec stage run by `gremlins land`
  interpolation:
    PR_URL: content("artifact://pr-url.txt")
  options:
    cmds:
      - gh pr merge --squash --delete-branch "{PR_URL}"

stages:
  - type: gremlins:plan
    prompt: [code-style, gremlins:plan.md]
```

| Key | Description |
|---|---|
| `default_client` | **Required.** `provider:model` string used for stages without an explicit `client:` |
| `base_ref` | Git ref to branch worktrees from. Defaults to `"current"`. |
| `github_integration` | If true, fetches `origin/<branch>` before creating worktrees and enables `gh` CLI use. |
| `bootstrap` | CLI source flags, launch-only commands, per-worktree commands, and `cli_out` artifact bindings. See [Bootstrap block](#bootstrap-block). |
| `prompts` | Named prompt map. Each key maps to a prompt string or list; referenced by name in stage `prompt:` fields. |
| `prompt_dir` | Directory that bare-name `prompt:` paths resolve against, relative to the YAML file. Defaults to the YAML's directory. |
| `stage-definitions` | Named reusable stage patterns. Values can be inline dicts or `gremlins:recipe` references. |
| `land` | An `exec` stage run by `gremlins land` (e.g. `gh pr merge`). See [Land block](#land-block). |
| `stages` | Ordered list of stage entries or parallel groups |

**Per-stage keys:**

| Key | Description |
|---|---|
| `name` | Unique stage identifier; used for `resume` targeting |
| `type` | Stage type — a primitive (`agent`, `exec`, `loop`, `parallel`, `sequence`), a bundled recipe (`gremlins:plan`, `gremlins:implement`, etc.), or a `stage-definitions` key |
| `client` | `provider:model` string; overrides `default_client` for this stage |
| `prompt` | Path or list of paths. `gremlins:NAME` resolves from the bundled package prompts; a bare `NAME` resolves from the pipeline's `prompt_dir`. |
| `options` | Free-form dict passed to the stage |
| `skip_if_exists` | Artifact key; if this artifact is verified to exist, skip the stage |
| `interpolation` | Map of variable names to registry key lookups: URI strings, `content("URI")` expressions, and optional `?default` fallbacks (see [Artifact binding](#artifact-binding)) |
| `bind` | Map of local variable names to URI strings; bound files are registered under the URI and the key is available for `{var}` substitution within the same stage (see [Artifact binding](#artifact-binding)) |
| `body` | List of child stages (for `loop` and `sequence` types) |
| `max-iterations` | Max loop iterations (for `loop` type; also settable via `options.max_iterations`) |
| `stop_when_exists` | Artifact key that terminates the loop when bound (for `loop` type) |
| `max_concurrent` | Max simultaneously running children (for `parallel` groups) |
| `cancel_on_bail` | If true, cancel outstanding parallel children when one bails (default: false) |
| `bail_policy` | `"any"` (default) or `"all"` — when to halt the parallel group on child bail |

**Client precedence:** CLI `--client` beats per-stage `client:`; per-stage `client:` beats pipeline `default_client:`.

**Parallel-group form:**

```yaml
- name: reviews
  parallel:
    - name: review-detail
      type: review-code
      client: xai:grok-4
    - name: review-security
      type: review-code
      client: xai:grok-4
  max_concurrent: 2         # optional; defaults to all children at once
```

| Key | Description |
|---|---|
| `name` | Group identifier |
| `parallel` | List of child stage entries (no nesting allowed) |
| `max_concurrent` | Max simultaneously running children (optional) |

### Client specifiers

Clients are specified as `provider:model` inline strings, either at the pipeline level (`default_client:`) or per stage (`client:`).

```yaml
default_client: xai:grok-4     # all stages default to this
stages:
  - name: plan
    type: gremlins:plan
  - name: implement
    type: gremlins:implement
    client: openai:gpt-4o      # this stage uses openai instead
```

Providers: `openai`, `xai`, `openrouter`, `cmd`. The CLI `--client provider:model` flag overrides the pipeline-level `default_client:` but yields to per-stage `client:` settings.

### `prompt:` field

```yaml
prompt: gremlins:plan.md                                  # single bundled file
prompt: [gremlins:code_style.md, plan.md]                 # mix bundled and local; concatenated with \n\n
```

Each entry is one of:

- `gremlins:NAME` — resolved from the bundled prompts shipped with the
  package. Use this for prompts owned by gremlins (`code_style.md`,
  `plan_gh.md`, etc.).
- bare `NAME` — resolved from the pipeline's top-level `prompt_dir:`
  (relative to the YAML file; defaults to the YAML's own directory). Use
  this for prompts you author and check in alongside your pipeline.

Lists are joined with `\n\n` before being passed to the stage. There is
no search fallback between the two — the prefix is the contract, so a
custom YAML reads as self-describing about which prompts come from the
package vs which must be provided locally.

By convention, project-local prompts live in `./.gremlins/prompts/` (a peer
of `./.gremlins/pipelines/`, not nested under it) and pipelines set
`prompt_dir: ../prompts`.

### `options:` field

A free-form dict passed verbatim to the stage. Selected options by stage
(see [`gremlins/stages/AGENTS.md`](gremlins/stages/AGENTS.md) for the full list):

**`verify`** — runs a list of shell commands with an agent fix-loop:

```yaml
options:
  cmds: ["make check", "make test"]  # commands to run (joined with &&)
  max_iterations: 3                  # fix-loop retries (default: 3)
```

**`agent`** — supports `options.model` to override the pipeline-default model for that stage (used by the `handoff` recipe's `model: haiku`).

### Stage types: primitives

Five primitive stage types are built into the engine (`gremlins/pipeline/loader.py`):

| Type | Description |
|---|---|
| `agent` | Resolves `interpolation:` artifacts, renders prompt, invokes the agent, verifies `bind:` artifacts |
| `exec` | Runs shell commands (`options.cmds` joined with `&&`) with `interpolation:`/`bind:` artifact bindings |
| `loop` | Iterates `body` stages until `stop_when_exists` is bound or `max-iterations` is exhausted |
| `parallel` | Fan-out/fan-in: runs `parallel:` children concurrently (up to `max_concurrent`) |
| `sequence` | Runs `body` stages sequentially using child state |

### Stage types: bundled recipes

Everything else is a bundled YAML recipe under `gremlins/recipes/stages/` that the
preprocessor auto-resolves by type name. Use them as `type: gremlins:<name>` or simply
as `type: <name>` — the preprocessor checks recipe names when no primitive or
`stage-definitions:` key matches, so bare `type: review-code`, `type: plan`, etc.
work without the `gremlins:` prefix.

| Recipe type | Recipe file | Description |
|---|---|---|
| `gremlins:plan` | `plan.yaml` | Local planning: agent writes `plan.md` + set-description |
| `gremlins:plan-gh` | `plan_gh.yaml` | GitHub planning: agent writes plan, publishes as issue, sets description |
| `gremlins:implement` | `implement.yaml` | Implementation: agent + git-commit + progress guard |
| `gremlins:review-code` | `review_code.yaml` | Code review agent, writes `{name}-{model}.md` |
| `gremlins:verify` | `verify.yaml` | Run commands, fix loop, bail on exhaustion |
| `gremlins:handoff` | `handoff.yaml` | Boss-loop chain manager: handoff agent + signal translation + sanitize |
| `gremlins:github-open-pr` | `github_open_pr.yaml` | Compose PR title/body, push branch, open PR |
| `gremlins:github-push-to-pr-branch` | `github_push_to_pr_branch.yaml` | Push HEAD to existing PR branch |
| `gremlins:github-request-copilot-review` | `github_request_copilot_review.yaml` | Add Copilot as PR reviewer |
| `gremlins:github-wait-copilot` | `github_wait_copilot.yaml` | Poll until Copilot posts a non-pending review |
| `gremlins:github-wait-ci` | `github_wait_ci.yaml` | Poll CI checks, fix loop, bail on exhaustion or `REVIEW_REQUIRED` |

Recipes with `required-prompt: true` (`plan`, `plan-gh`, `implement`, `verify`, `github-wait-ci`) must receive a `prompt:` at the call site. Recipes with `required-options` (`verify` requires `cmds`) must receive those options.

### Bootstrap block

The `bootstrap:` top-level key controls CLI flags and setup commands:

```yaml
bootstrap:
  source:
    plan:
      type: [filepath, string]
      optional: true
    instructions:
      type: string
      optional: true
  launch_cmds:
    - gremlins:bind_artifact(plan, plan, file://session/plan.md)
  cmds:
    - "uv sync"
  cli_out:
    pr: "opaque://pr/{read:pr-num}"
```

| Key | Description |
|---|---|
| `source` | Declares CLI flags. Each key becomes a `--<key>` flag (required unless `optional: true`). Supported types: `filepath`, `string`. |
| `launch_cmds` | Shell commands run once at launch. Supports the `gremlins:bind_artifact(source_key, artifact_key, uri)` DSL for resolving source values into artifacts. |
| `cmds` | Shell commands run in every worktree (e.g. `uv sync` to set up the dev environment). |
| `cli_out` | Artifact bindings computed at launch from source values (e.g. binding an `opaque://pr/{read:pr-num}` URI from a `--pr-num` flag). |

The `gremlins:bind_artifact` DSL resolves a source value (GitHub issue ref, filepath, or inline text) and binds it as an artifact in the registry. GitHub issue refs (`#N` or `owner/repo#N`) are downloaded via `gh issue view`.

> **Note:** Per-worktree bootstrap commands now live exclusively in the pipeline's `bootstrap.cmds`. The old `.gremlins/bootstrap.yaml` overlay file is no longer read — move any entries there into the `bootstrap.cmds` block of your pipeline YAML.

### Land block

The `land:` top-level key defines an `exec` stage run by `gremlins land`. It replaces the default land behavior (squash-merge or fast-forward) with a custom command:

```yaml
land:
  interpolation:
    PR_URL: content("artifact://pr-url.txt")
  options:
    cmds:
      - gh pr merge --squash --delete-branch "{PR_URL}"
```

When a pipeline declares `land:`, `gremlins land` runs this stage instead of the built-in merge logic. The stage runs in the project root (not the worktree).

### Parallel groups

Wrap sibling stages in a `parallel:` list to run them concurrently:

```yaml
default_client: xai:grok-4

stages:
  - type: gremlins:plan
    prompt: [code-style, gremlins:plan.md]

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
      - name: review-security
        type: review-code
    max_concurrent: 2

  - name: address-code
    type: agent
```

**Execution and failure:** The parallel group executes in three phases:
1. **Fan-out** — each child stage starts independently as a subprocess
2. **Concurrent execution** — all children run simultaneously (up to `max_concurrent`)
3. **Fan-in** — all children finish or one bails; siblings continue running until group completion

If any child fails (raises `Bail`), the pipeline halts after the group finishes —
siblings are not cancelled mid-run by default. This can be changed with `cancel_on_bail: true`
to cancel outstanding tasks immediately. The bail is evaluated via `bail_policy` (default: `any`,
meaning one failed child halts the group; set `bail_policy: all` to halt only when all children bail).
Subsequent stages are skipped; the operator can resume or ack the group via CLI.

**State isolation:** Each child gets its own state directory and subprocess.
Client overrides, worktree paths, and artifact bindings are isolated per-child.
Children run in parallel without blocking each other. Parent `state.json` is updated
during the concurrent phase (e.g., `active_children` snapshot); copying child artifact
bindings into the parent registry is deferred until fan-in completes.

**Resume targeting:** Use the full child gremlin ID (form: `<parent-id>--<group-name>--<child-key>`,
visible in fleet view) to resume a specific child. Resuming the parent group ID re-spawns all
children that haven't landed.

**Base ref propagation:** Child worktrees are derived from the parent's `base_ref` as recorded in state.

### Worked example: project-local override

Create `.gremlins/pipelines/local.yaml` to override the bundled `local`
pipeline. This example adds a `verify` stage before `review-code` and
overrides the client for the address stage:

```yaml
default_client: xai:grok-4

stages:
  - { type: gremlins:plan,       prompt: [code-style, gremlins:plan.md] }
  - { type: gremlins:implement,  prompt: [code-style, gremlins:implement_local.md] }
  - { type: verify,              options: { cmds: ["pytest"] }, prompt: verify }
  - { type: review-code }
  - { name: address-code, type: agent, client: openai:gpt-4o, prompt: [code-style, gremlins:address.md, gremlins:bail_section.md], interpolation: {text: review-code} }
```

Add a `prompt:` key to any stage to supply a custom prompt; paths are
relative to the YAML file. `review-code` uses a fixed prompt and ignores
per-stage `prompt:` overrides.

### Worked example: parallel reviewers

Run two `review-code` passes in parallel, then address both:

```yaml
default_client: xai:grok-4

stages:
  - { type: gremlins:plan, prompt: [code-style, gremlins:plan.md] }
  - { type: gremlins:implement, prompt: [code-style, gremlins:implement_local.md] }

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
      - name: review-security
        type: review-code
    max_concurrent: 2

  - { name: address-code, type: agent, prompt: [code-style, gremlins:address.md, gremlins:bail_section.md], interpolation: {text: review-code} }
```

### Stage definitions

YAML `stage-definitions:` lets you name and reuse stage patterns within a pipeline:

```yaml
stage-definitions:
  review-base: &review-base
    type: review-code
    client: xai:grok-4
    prompt: gremlins:code_style.md

stages:
  - { type: gremlins:plan, prompt: [code-style, gremlins:plan.md] }
  - { type: gremlins:implement, prompt: [code-style, gremlins:implement_local.md] }
  - name: review-detail
    <<: *review-base
    prompt: [gremlins:code_style.md, detail_review.md]
  - name: review-security
    <<: *review-base
    prompt: security_review.md
```

Definitions provide base `type`, `options`, and `prompt`. Call-sites can override
`prompt` and `options` via YAML anchors (as shown above) or via template placeholders
in multi-stage recipes. Call-sites own the `name:`, `interpolation:`, and `bind:` keys;
`bind:` is forbidden inside a definition, but `interpolation:` can be declared and will be
merged with call-site `interpolation:` values. For single-stage definitions, only `name`, `interpolation`,
and `bind` keys can be safely overridden; to vary `prompt` or `options`, use anchors.

### Artifact binding

Stages can bind artifacts via `interpolation:` and `bind:` maps. These define what data
flows between stages in the pipeline:

```yaml
stages:
  - name: scan
    type: exec
    options:
      cmds: ["python scan.py > \"{report}\""]
    bind:
      report: file://session/report

  - name: analyze
    type: agent
    interpolation:
      report: content("file://session/report")
    prompt: |
      The scanning report:
      {report}

      Propose fixes.
```

**Artifact URI schemes:**
- `file://session/<name>` — Session artifact: a file created under the gremlin's artifact directory
- `git://ref/<name>` — Git ref name (e.g., `git://ref/main` returns the string `main`)
- `git://commit/<sha>` — Commit SHA (e.g., `git://commit/abc123def` returns the full SHA)
- `git://range/<base>..<head>` — Commit range/log between two refs
- `opaque://pr/<n>` — Opaque PR identifier. Resolution returns `{"uri": "opaque://pr/<n>"}`
  (the URI string itself) without calling external tools; downstream stages pass it to shell
  commands that need the PR number (e.g., `${uri##*/}` to extract `<n>`)
- `git://range` — Special shorthand: the `exec` stage snapshots HEAD before running and binds the resulting range afterwards

**Artifact binding semantics:**
- `interpolation:` values are registry key lookups: a URI string (e.g., `file://session/report`), an optional `?default` fallback (e.g., `mykey?fallback`), or a `content("URI")` expression that reads and inlines file contents
- `bind:` values are URI strings naming what the stage produces; the bind map key is a local variable name for `{var}` substitution within the same stage's templates (prompts, cmds)
- After a stage completes, bound artifacts are registered under their URI strings; downstream stages reference those URI strings in their `interpolation:` maps
- `interpolation:` can be declared in a stage definition and will be merged with call-site `interpolation:` values; `bind:` cannot appear inside a definition

### Stage definitions and bundled recipes

Some stage types are not built-in — they are provided as bundled YAML recipes and must be wired in via `stage-definitions:` before use:

```yaml
stage-definitions:
  github-push-to-pr-branch: gremlins:github_push_to_pr_branch

stages:
  - { name: push, type: github-push-to-pr-branch }
```

`gremlins:NAME` resolves the recipe from the bundled package (`gremlins/recipes/stages/NAME.yaml`). A bare path resolves relative to the pipeline file.

### Bundled pipelines

The canonical reference pipelines:

- [`gremlins/pipelines/local.yaml`](gremlins/pipelines/local.yaml) — `gremlins launch local`
- [`gremlins/pipelines/gh.yaml`](gremlins/pipelines/gh.yaml) — `gremlins launch gh`
- [`gremlins/pipelines/gh-terse.yaml`](gremlins/pipelines/gh-terse.yaml) — `gremlins launch gh-terse`
- [`gremlins/pipelines/pr-extend.yaml`](gremlins/pipelines/pr-extend.yaml) — `gremlins launch pr-extend`
- [`gremlins/pipelines/boss.yaml`](gremlins/pipelines/boss.yaml) — `gremlins launch boss`

## Error handling and recovery

Gremlins can fail or get stuck during execution. Understanding how to recover is essential for running long-running pipelines.

### Bail semantics

When a stage detects an unrecoverable condition (e.g., a code review requests changes, secrets are detected, or a merge conflict blocks progress), it raises a `Bail` exception with a detail string.

By convention, agent-based stages emit a `BAIL: <class>: <detail>` marker at the end of their output. The `<class>` token is conventionally one of:
- `reviewer_requested_changes` — code review found issues that must be addressed
- `security` — security review detected problems
- `secrets` — credentials or sensitive data detected in the code
- `other` — stage-specific or unknown failure condition

The bail detail is written to a per-attempt `bail_<attempt>.json` file in the gremlin's state directory and is visible in the fleet view. When a stage bails, the entire pipeline halts — subsequent stages do not run, but the gremlin's state is preserved for recovery.

### Recovering from gremlin failures

When a gremlin bails and halts, you have three recovery options:

**`gremlins resume <id>`** — Re-spawn the bailed gremlin from the stage where it
bailed. Use this when the cause has been fixed externally (e.g., a code review
fix has been merged, or a merge conflict has been resolved). The gremlin will
restart from the bailed stage with the current worktree state.

**`gremlins ack <id>`** — Acknowledge the gremlin without re-running. Use this
when the bailed condition is acceptable (e.g., the review found minor style
issues that don't block landing, or external work was already completed). The
gremlin marks the bailed stage as complete and proceeds to subsequent stages.

**`gremlins skip <id>`** — Create a new sibling attempt with the same parameters
and a fresh ID, leaving the failed gremlin in place. Use this for transient
failures (timeouts, CI hangs) that won't self-resolve. Both attempts are visible
in the fleet; the new attempt begins from the start.

### Handling parallel group failures

When a child in a parallel group bails:
- The group halts after all currently-running children finish (not mid-run), unless `cancel_on_bail: true`
- The bail reason is attributed to the child stage name
- `gremlins resume <parent-id>` re-spawns all children that haven't landed
- `gremlins resume <parent-id>--<group-name>--<child-key>` resumes only that child (use the full child ID from fleet view)

If the cause was a transient failure affecting multiple children, `skip` the entire
group and re-launch the pipeline to restart all children.

### Boss-chain recovery

When a boss gremlin spawns child gremlins (`gremlins launch ... --parent <boss-id>`),
the boss halts if a child bails. At this point:
- The child's gremlin ID is visible in the fleet view as a child of the boss
- Recover the child (`resume`, `ack`, or `skip`) independently
- Once the child lands or is abandoned, resume the boss (`gremlins resume <boss-id>`)

The boss resumes from its child-spawn stage and proceeds with the next iteration
(re-planning, re-implementing, or wrapping up, depending on the pipeline).

## Environment variables

### Runtime behaviour

| Variable | Default | Description |
|---|---|---|
| `GREMLINS_REASONING_EFFORT` | *(unset)* | Reasoning effort for OpenAI-compatible backends (`openai`, `xai`, `openrouter`). One of `low`, `medium`, `high`. Unset disables reasoning entirely (no token cost). Only takes effect for models that support reasoning. Per-client `reasoning=` params override this default — see [Client specifier syntax](#client-specifier-syntax) below. |
| `GREMLINS_STREAM_IDLE_TIMEOUT` | `600` | Stream idle timeout in seconds for OpenAI-compatible backends. If the model produces no output for this duration the stream is cancelled and the call is retried. |
| `GREMLINS_OPENAI_AGENTS_MAX_TURNS` | `1000` | Maximum agent loop turns for OpenAI-compatible backends. Guards against runaway tool-call loops. |
| `GREMLINS_LOG_LEVEL` | `INFO` | Log level for gremlins output. One of `DEBUG`, `INFO`, `WARNING`, `ERROR`. |

### Client specifier syntax

Client specifiers follow the format `provider:model` with an optional `:k=v,...`
params suffix:

```
provider:model:k1=v1,k2=v2,...
```

**Examples:**

```
xai:grok-4:reasoning=high
openrouter:deepseek/deepseek-v4-pro:reasoning=high,thinking=deepseek
openai:gpt-4o-mini:temperature=0.7
cmd:claude -p --model sonnet
```

Params are passed directly to the model backend as additional request
parameters. The `reasoning` param gets special handling — it is expanded
into the nested `{"effort": "<value>", "summary": "auto"}` object expected
by OpenAI-compatible reasoning APIs. All other params are forwarded as
literal values.

- A per-client `reasoning=` param overrides `GREMLINS_REASONING_EFFORT` for
  that client.
- The `cmd` provider does not parse params — the full string after `cmd:` is
  treated as the shell command.
- Reserved keys `reasoning` and `parallel_tool_calls` are handled by the
  backend and should not be set directly by the user.

### Filesystem overrides

These are primarily for testing but can be used to redirect gremlins I/O:

| Variable | Description |
|---|---|
| `GREMLINS_SANDBOX_ROOT` | Re-bases `state_root()`, `work_root()`, and `user_config_root()` under a single directory. When set, all gremlin state and worktrees live under this path. |
| `GREMLINS_PROJECT_ROOT` | Overrides the project root directory (normally the cwd at launch time). |
| `GREMLINS_OVERLAY_DIR` | Overrides the `.gremlins` config directory for pipeline/prompt discovery. |

### Internal (set by gremlins itself)

These are set by the launcher or executor and should not be set manually:

| Variable | Set by | Description |
|---|---|---|
| `GREMLINS_GREMLIN_ID` | Launcher | The current gremlin's unique ID. Stages and state bookkeeping no-op without it. |
| `GREMLINS_WORKTREE_PATH` | Executor | Path to the gremlin's git worktree. |
| `GREMLINS_ARTIFACT_DIR` | Executor | Path to the gremlin's artifact directory. |
| `GREMLINS_RESUME_FROM` | CLI | Stage name to resume from. |
| `GREMLINS_CWD_OF_CLI_CMD` | CLI | Working directory for CLI-spawned commands. |
| `GREMLINS_BOOTSTRAP_CWD` | Launcher | The original cwd captured at launch time. |

## What can a gremlin do to my machine?

Gremlins are restricted to an allowlist of tools (Read, Edit, Write, Bash,
Grep, Glob) and their Bash commands are path-scoped to the gremlin's git
worktree. They can read and modify files inside that worktree and block direct
path references outside it. This is a best-effort token check, not a full
sandbox — indirect references (heredocs, computed paths) may not be caught.

**Honest disclaimer**: The allowlist limits *reach* — what paths and tools the
agent can invoke. It does not limit *impact within reach*. A gremlin with
write access to your worktree can make any change inside it. Review landed
commits before merging.

**Backend differences**: On `openai:`, `xai:`, and `openrouter:` backends,
gremlins owns the tool layer and enforces worktree/cwd containment directly.
On `cmd:` backends, the gremlins-layer containment is **not** translated into
CLI flags or settings — the underlying command reads the operator's ambient
config and enforces whatever the operator has configured there. See "Backend
config inheritance" below.

### Backend config inheritance

The `cmd:` backend runs the specified command as a subprocess. It does *not*
materialize a per-gremlin config dir. Whatever the operator has configured
for their interactive session is exactly what the subprocess sees:

- **Settings** — the subprocess reads whatever config files it normally would
  (e.g. `~/.claude/settings.json` for `cmd:claude`). The gremlins-layer
  `allowed_tools` / `disallowed_tools` block has no effect on `cmd:` runs;
  configure tool permissions via your own settings.

  Gremlin worktrees — where the `cmd:` subprocess does its file edits —
  live under a stable, gremlins-scoped prefix in the system temp directory.
  Discover it at runtime:

  ```
  python -c "from gremlins import paths; print(paths.work_root())"
  ```

  On Linux/macOS this is `/tmp/gremlins`; the OS reclaims orphaned
  worktrees on reboot.
- **MCP servers and hooks** — inherited from the user's config.
- **Auth** — follows whatever auth the subprocess command normally uses.

### Local environment overrides

If `.gremlins/env` exists in the project root, gremlins sources it through
`bash` at startup and merges any new or changed variables into the process
environment before any stage runs. All subprocesses (plan, implement, verify,
review) inherit the result automatically.

> **Security warning:** because `.gremlins/env` is executed as a bash script,
> it can run arbitrary code. Do not run gremlins in a repository unless you
> have reviewed the contents of `.gremlins/env` and trust them.

The file is sourced via `bash`, so it can use command substitution,
conditionals, and anything bash supports:

```sh
export VIRTUAL_ENV=$(poetry env info --path)
export PATH="$VIRTUAL_ENV/bin:$PATH"
export TEST_DATABASE_URL=postgresql://localhost/mydb_test
```

Add `.gremlins/env` to your `~/.gitignore_global` or project `.gitignore`.

### Loader API

- `gremlins/pipeline/__init__.py::Pipeline.from_yaml(path)` — loads and expands a pipeline YAML file, validates duplicate producers, fills stage clients.
- `gremlins/pipeline/loader.py` — `STAGE_TYPES` (primitive type → class map), `parse_stages`, `parse_stage`, `fill_names`, `check_duplicate_producers`.
- `gremlins/pipeline/discovery.py` — `resolve_pipeline_path`, `resolve_pipeline_name`, `list_pipelines`.
- `gremlins/pipeline/preprocess.py` — `expand_pipeline` (include/prompt/recipe/stage-definition expansion).

## Internals docs

- [`gremlins/AGENTS.md`](gremlins/AGENTS.md) — module layout, entry points,
  testability seam, byte-stable strings
- [`gremlins/clients/AGENTS.md`](gremlins/clients/AGENTS.md) — client backend internals
- [`gremlins/fleet/AGENTS.md`](gremlins/fleet/AGENTS.md) — fleet manager internals
- [`gremlins/pipelines/AGENTS.md`](gremlins/pipelines/AGENTS.md) — pipeline configuration internals
- [`gremlins/stages/AGENTS.md`](gremlins/stages/AGENTS.md) — stage internals