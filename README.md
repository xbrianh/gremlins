# gremlins

Background coding-agent pipelines that plan, implement, review, and land work
end-to-end. Given a goal or GitHub issue, a gremlin runs the full
plan → implement → review-code → address-code cycle unattended, writing
artifacts to the per-user state directory resolved by
`platformdirs.user_state_dir("gremlins")` and optionally opening a pull
request. A fleet manager tracks running, stalled, and finished gremlins and
provides stop / rescue / land / close operations.

**Status: brand-new and a bit janky.** This is a fresh project, actively
shaped by daily use. Expect rough edges — stream timeouts, the occasional
merge conflict from parallel gremlins, a few stages still finding their
final shape. Bug reports, ideas, and PRs are all welcome.

---

## Using gremlins with a coding assistant

Paste the output of `gremlins prompt-for-assistant` into a fresh Claude Code session (or any compatible assistant) to configure it as a competent gremlins collaborator.

The workflow: you discuss the work with the assistant, it captures discrete units as GitHub issues or plan files, launches gremlins in the background to implement them, and lands each finished gremlin before starting dependent work. You stay at the strategic level — deciding what to build and in what order — while gremlins handle the implementation cycle unattended. The assistant maintains a queue of running, pending, and blocked work and surfaces it on request.

---

## Prerequisites

- `gh` — GitHub CLI: https://github.com/cli/cli#installation
- `git` — Git (pre-installed on most systems)
- `claude` — Claude Code CLI: https://docs.anthropic.com/en/docs/claude-code

## Dev install

```sh
uv venv
source .venv/bin/activate  # or `.venv\Scripts\activate` on Windows
uv pip install -e ".[dev]"
```

## Make targets

| Target | What it runs |
|---|---|
| `make test` | `pytest` |
| `make lint` | `ruff check .` |
| `make format` | `ruff format --check .` (check only — does not rewrite files) |
| `make typecheck` | `pyright` |
| `make check` | lint + format + typecheck |

## CLI subcommands

Invoked as `python -m gremlins.cli <subcommand>` or `gremlins <subcommand>`
after install. The authoritative list and per-subcommand description lives in
the dispatch table in [`gremlins/cli/__init__.py`](gremlins/cli/__init__.py).

| Subcommand | Purpose |
|---|---|
| `launch <name>` | Launch a background gremlin by pipeline name (`gremlins launch --list` to see available) |
| `resume` | Re-spawn an existing gremlin from its recorded stage |
| `stop` | Send SIGTERM to a running gremlin |
| `rescue` | Diagnose and resume a dead or stalled gremlin |
| `land` | Land a finished gremlin onto the current branch |
| `rm` | Delete a dead gremlin's state dir, worktree, and branch |
| `close` | Mark a dead gremlin as closed |
| `log` | Tail the gremlin's log file |
| `prompt-for-assistant` | Print the assistant setup prompt to stdout |

`_run-pipeline` is an internal spawn boundary; not for direct use.

### Launch flags

#### Per-pipeline flags

Flags vary by pipeline. The first stage's `__init__` signature defines the accepted flags; `gremlins launch <name> --help` prints the full list.

Common infrastructure flags (accepted by all pipelines):

| Flag | Default | Description |
|---|---|---|
| `--plan <path-or-ref>` | — | Path to a plan/spec file, or a GitHub issue ref (`42`, `#42`, `owner/repo#42`, or issue URL) |
| `--description <text>` | — | Human-readable description stored in state |
| `--parent <id>` | — | Parent gremlin ID (used by boss to track child ownership) |
| `--print-id` | false | Print the gremlin ID to stdout after launch |
| `-c`/`--instructions <text>` | — | Instructions string (mutually exclusive with `--plan`) |
| `--base-ref <ref>` | `HEAD` | Git ref to branch the worktree from; ignored for gh pipelines (always anchors to origin default branch) |
| `--spec <path>` | — | Path to a coding-style spec file passed into stages |

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
name: my-pipeline         # optional; defaults to the file stem

default_client: claude:sonnet   # optional; provider:model string

prompt_dir: ../prompts          # optional; relative to YAML, defaults to the YAML's directory

stages:
  - name: plan
    type: plan
    client: copilot:gpt-5.4     # optional; overrides default_client for this stage
    prompt: gremlins:plan.md    # `gremlins:NAME` -> bundled prompts; bare NAME -> prompt_dir
    options: {}
```

| Key | Description |
|---|---|
| `name` | Pipeline display name; defaults to the file stem |
| `default_client` | `provider:model` string used for stages without an explicit `client:` |
| `prompt_dir` | Directory that bare-name `prompt:` paths resolve against, relative to the YAML file. Defaults to the YAML's directory. |
| `stages` | Ordered list of stage entries or parallel groups |

**Per-stage keys:**

| Key | Description |
|---|---|
| `name` | Unique stage identifier; used for `resume` targeting |
| `type` | Registered stage type (see [Available stage types](#available-stage-types)) |
| `client` | `provider:model` string; overrides `default_client` for this stage |
| `prompt` | Path or list of paths. `gremlins:NAME` resolves from the bundled package prompts; a bare `NAME` resolves from the pipeline's `prompt_dir`. |
| `options` | Free-form dict passed to the stage |

**`provider:model` format:**

Providers: `claude` (default), `copilot`. The model part is optional — `claude:` and `claude:sonnet` are both valid. Examples: `claude:sonnet`, `copilot:gpt-5.4`, `claude:`. Per-stage `client:` in YAML takes precedence over the CLI `--client` flag; `default_client:` at the pipeline level does not.

**Parallel-group form:**

```yaml
- name: reviews
  parallel:
    - name: review-detail
      type: review-code
      client: claude:sonnet
    - name: review-security
      type: review-code
      client: claude:sonnet
  max_concurrent: 2         # optional; defaults to all children at once
```

| Key | Description |
|---|---|
| `name` | Group identifier |
| `parallel` | List of child stage entries (no nesting allowed) |
| `max_concurrent` | Max simultaneously running children (optional) |

### Client specifiers

Clients are specified as `provider:model` inline strings, either at the pipeline level (`default_client:`) or per stage (`client:`). The model part is optional.

```yaml
default_client: claude:sonnet     # all stages default to this
stages:
  - name: plan
    type: plan
  - name: implement
    type: implement
    client: copilot:gpt-5.4       # this stage uses copilot instead
```

Providers: `claude`, `copilot`. The CLI `--client provider:model` flag overrides the pipeline-level `default_client:` but yields to per-stage `client:` settings.

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

**`verify`** — runs `check_cmd` then `test_cmd`, with an agent fix-loop:

```yaml
options:
  check_cmd: make check   # lint/typecheck command (optional)
  test_cmd: make test     # test command (optional)
  max_attempts: 3         # fix-loop retries (default: 3)
```

**`test`** — runs a single test command, with an agent fix-loop:

```yaml
options:
  test_cmd: pytest        # falls back to --test CLI flag; stage no-ops if unset in both
  max_attempts: 3         # fix-loop retries (default: 3)
```

For `local` stages, model options (`plan_model`, `impl_model`, `address_model`,
`test_fix_model`, `detail`) can also be set here to override the CLI defaults.

### Available stage types

| Type | Description |
|---|---|
| `plan` | Produces an implementation plan |
| `implement` | Applies the plan to the working tree |
| `review-code` | Runs a code review and writes findings to disk |
| `address-code` | Applies code-review findings |
| `verify` | Runs check and test commands with an agent fix-loop |
| `test` | Runs a single test command with an agent fix-loop |
| `request-copilot` | Requests a Copilot review on the open PR |
| `github-review-pull-request` | Runs the `/ghreview` skill against the open PR |
| `github-wait-copilot` | Polls until Copilot posts its review |
| `github-address-pull-request-reviews` | Runs the `/ghaddress` skill to address PR review comments |
| `wait-ci` | Polls PR CI checks until they pass or exhaust attempts |

### Parallel groups

Wrap sibling stages in a `parallel:` list to run them concurrently:

```yaml
default_client: claude:sonnet

stages:
  - name: plan
    type: plan

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
      - name: review-security
        type: review-code
    max_concurrent: 2

  - name: address-code
    type: address-code
```

If any child fails, the pipeline halts after the group finishes — siblings
are not cancelled mid-run. `gremlins resume` accepts both the group name
(`reviews`) and individual child names (`review-detail`).

### Worked example: project-local override

Create `.gremlins/pipelines/local.yaml` to override the bundled `local`
pipeline. This example uses Opus for plan/implement/address stages and adds
a `test` stage before `review-code`:

```yaml
name: local

stages:
  - { type: plan,         options: { plan_model: opus } }
  - { type: implement,    options: { impl_model: opus } }
  - { type: test,         options: { test_cmd: pytest } }
  - { type: review-code }
  - { type: address-code, options: { address_model: opus } }
```

Add a `prompt:` key to any stage to supply a custom prompt; paths are
relative to the YAML file.

### Worked example: parallel reviewers

Run two `review-code` passes in parallel, then address both:

```yaml
name: local

default_client: claude:sonnet

stages:
  - { type: plan }
  - { type: implement }

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
      - name: review-security
        type: review-code
    max_concurrent: 2

  - { type: address-code }
```

Note: `review-code` does not currently support per-stage prompt overrides
via YAML — both passes use the built-in detail lens.

### Bundled pipelines

The canonical reference pipelines:

- [`gremlins/pipelines/local.yaml`](gremlins/pipelines/local.yaml) — `gremlins launch local`
- [`gremlins/pipelines/gh.yaml`](gremlins/pipelines/gh.yaml) — `gremlins launch gh`
- [`gremlins/pipelines/boss.yaml`](gremlins/pipelines/boss.yaml) — `gremlins launch boss`

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

`gremlins/pipeline.py` exposes:

- `load_pipeline(path)` → `Pipeline` — parses a YAML file, resolves `clients`
  via `CLIENT_FACTORIES`, and validates every stage `type` against
  `STAGE_REGISTRY` (populated by importing `gremlins.stages.all`).
- `resolve_pipeline_path(name_or_path, base_dir)` — resolves a name or path
  using the discovery order above.

Dataclasses: `Pipeline`, `StageEntry` (parallel groups have `type="parallel"`
internally and carry a `children` list and optional `max_concurrent`).

## Internals docs

- [`gremlins/AGENTS.md`](gremlins/AGENTS.md) — module layout, entry points,
  testability seam, byte-stable strings
- [`gremlins/fleet/AGENTS.md`](gremlins/fleet/AGENTS.md) — fleet manager internals
- [`gremlins/orchestrators/AGENTS.md`](gremlins/orchestrators/AGENTS.md) — orchestrator internals
- [`gremlins/stages/AGENTS.md`](gremlins/stages/AGENTS.md) — stage internals

## Planned: `gremlins install`

A future `gremlins install` subcommand will bootstrap the Claude Code skill
layer — syncing this package into `~/.claude/gremlins/` and wiring up the
hook and skill definitions. **This command does not exist yet.**
