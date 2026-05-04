# gremlins

Background orchestration pipelines for Claude Code. Given a goal or GitHub issue,
a gremlin runs the full plan → implement → review-code → address-code cycle
unattended, writing artifacts to `~/.local/state/claude-gremlins/` and optionally
opening a pull request. A fleet manager tracks running, stalled, and finished
gremlins and provides stop / rescue / land / close operations.

**Status:** pre-1.0, not published to PyPI. The copy at `~/.claude/gremlins/`
is still what Claude Code skills (`/localgremlin`, `/ghgremlin`, `/bossgremlin`)
consume today — this repo is the upstream source.

---

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
the module docstring at the top of [`gremlins/cli.py`](gremlins/cli.py).

| Subcommand | Purpose |
|---|---|
| `launch local` | Full local pipeline: plan → implement → review-code → address-code |
| `launch gh` | GitHub issue-driven pipeline (plan → implement → PR → Copilot review → address) |
| `launch boss` | Chained serial workflow driven by a top-level spec |
| `review` | review-code stage only |
| `address` | address-code stage only |
| `resume` | Re-spawn an existing gremlin from its recorded stage |
| `stop` | Send SIGTERM to a running gremlin |
| `rescue` | Diagnose and resume a dead or stalled gremlin |
| `land` | Land a finished gremlin onto the current branch |
| `rm` | Delete a dead gremlin's state dir, worktree, and branch |
| `close` | Mark a dead gremlin as closed |
| `log` | Tail the gremlin's log file |

`_run-pipeline` is an internal spawn boundary; not for direct use.

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

Defaults: `launch local` → `local`, `launch gh` → `gh`.

### Selecting a pipeline

```sh
gremlins launch local                                          # bundled local.yaml
gremlins launch local --pipeline my-pipeline                   # .gremlins/pipelines/my-pipeline.yaml
gremlins launch local --pipeline .gremlins/pipelines/foo.yaml  # direct path
gremlins launch gh --pipeline gh                               # bundled gh.yaml
```

### Schema reference

**Top-level keys:**

```yaml
name: my-pipeline         # optional; defaults to the file stem

clients:
  claude_sonnet:
    provider: claude
    model: sonnet

stages:
  - name: plan
    type: plan
    client: claude_sonnet
    prompt: prompts/plan.md
    options: {}
```

| Key | Description |
|---|---|
| `name` | Pipeline display name; defaults to the file stem |
| `clients` | Named client definitions |
| `stages` | Ordered list of stage entries or parallel groups |

**Per-stage keys:**

| Key | Description |
|---|---|
| `name` | Unique stage identifier; used for `resume` targeting |
| `type` | Registered stage type (see [Available stage types](#available-stage-types)) |
| `client` | Key from `clients:`; omit for stages that need no model |
| `prompt` | Path or list of paths, relative to the YAML file |
| `options` | Free-form dict passed to the stage |

**Parallel-group form:**

```yaml
- name: reviews
  parallel:
    - name: review-detail
      type: review-code
      client: claude_sonnet
    - name: review-security
      type: review-code
      client: claude_sonnet
  max_concurrent: 2         # optional; defaults to all children at once
```

| Key | Description |
|---|---|
| `name` | Group identifier |
| `parallel` | List of child stage entries (no nesting allowed) |
| `max_concurrent` | Max simultaneously running children (optional) |

### `clients:` block

```yaml
clients:
  claude_sonnet: { provider: claude, model: sonnet }
```

`provider` selects the client implementation; `model` is passed through.
Today only `claude` is available as a provider.

### `prompt:` field

```yaml
prompt: prompts/plan.md                                  # single file
prompt: [prompts/code_style.md, prompts/plan.md]         # list — concatenated with \n\n
```

Paths are relative to the YAML file. Lists are joined with `\n\n` before
being passed to the stage. There is no runtime templating.

To reuse bundled prompts from a project-local pipeline file, copy the files
you need from `gremlins/pipelines/prompts/` into your
`.gremlins/pipelines/prompts/` directory.

### `options:` field

A free-form dict passed verbatim to the stage. Stages with documented
options today:

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
  test_cmd: pytest        # required; stage no-ops if absent
  max_attempts: 3         # fix-loop retries (default: 3)
```

See [`gremlins/stages/CLAUDE.md`](gremlins/stages/CLAUDE.md) for the full
per-stage option schemas.

### Available stage types

| Type | Description |
|---|---|
| `plan` | Produces an implementation plan |
| `implement` | Applies the plan to the working tree |
| `review-code` | Runs a code review and writes findings to disk |
| `address-code` | Applies code-review findings |
| `verify` | Runs check and test commands with an agent fix-loop |
| `test` | Runs a single test command with an agent fix-loop |
| `commit-pr` | Commits changes and opens a pull request |
| `request-copilot` | Requests a Copilot review on the open PR |
| `ghreview` | Runs the `/ghreview` skill against the open PR |
| `wait-copilot` | Polls until Copilot posts its review |
| `ghaddress` | Runs the `/ghaddress` skill to address PR review comments |
| `wait-ci` | Polls PR CI checks until they pass or exhaust attempts |

### Parallel groups

Wrap sibling stages in a `parallel:` list to run them concurrently:

```yaml
stages:
  - name: plan
    type: plan
    client: claude_sonnet

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
        client: claude_sonnet
      - name: review-security
        type: review-code
        client: claude_sonnet
    max_concurrent: 2

  - name: address-code
    type: address-code
    client: claude_sonnet
```

If any child fails, the pipeline halts after the group finishes — siblings
are not cancelled mid-run. `gremlins resume` accepts both the group name
(`reviews`) and individual child names (`review-detail`).

### Worked example: project-local override

Create `.gremlins/pipelines/local.yaml` to override the bundled `local`
pipeline. This example swaps in Opus and runs `test` before `review-code`:

```yaml
name: local

clients:
  claude_opus: { provider: claude, model: opus }

stages:
  - { name: plan,         type: plan,         client: claude_opus }
  - { name: implement,    type: implement,    client: claude_opus }
  - { name: test,         type: test,         options: { test_cmd: pytest } }
  - { name: review-code,  type: review-code,  client: claude_opus }
  - { name: address-code, type: address-code, client: claude_opus }
```

Add a `prompt:` key to any stage to supply a custom prompt; paths are
relative to the YAML file.

### Worked example: parallel reviewers

Run two `review-code` passes in parallel — one for general detail, one for
security — then address both:

```yaml
name: local

clients:
  claude_sonnet: { provider: claude, model: sonnet }

stages:
  - { name: plan,      type: plan,      client: claude_sonnet }
  - { name: implement, type: implement, client: claude_sonnet }

  - name: reviews
    parallel:
      - name: review-detail
        type: review-code
        client: claude_sonnet
        prompt: prompts/detail.md
      - name: review-security
        type: review-code
        client: claude_sonnet
        prompt: prompts/security.md
    max_concurrent: 2

  - { name: address-code, type: address-code, client: claude_sonnet }
```

Prompt paths here are relative to `.gremlins/pipelines/`. Copy files from
`gremlins/pipelines/prompts/lenses/` into `.gremlins/pipelines/prompts/`
to reuse the bundled lenses.

### Bundled pipelines

The canonical reference pipelines:

- [`gremlins/pipelines/local.yaml`](gremlins/pipelines/local.yaml) — default for `launch local`
- [`gremlins/pipelines/gh.yaml`](gremlins/pipelines/gh.yaml) — default for `launch gh`

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

- [`gremlins/CLAUDE.md`](gremlins/CLAUDE.md) — module layout, entry points,
  testability seam, byte-stable strings
- [`gremlins/fleet/CLAUDE.md`](gremlins/fleet/CLAUDE.md) — fleet manager internals
- [`gremlins/orchestrators/CLAUDE.md`](gremlins/orchestrators/CLAUDE.md) — orchestrator internals
- [`gremlins/stages/CLAUDE.md`](gremlins/stages/CLAUDE.md) — stage internals

## Planned: `gremlins install`

A future `gremlins install` subcommand will bootstrap the Claude Code skill
layer — syncing this package into `~/.claude/gremlins/` and wiring up the
hook and skill definitions. **This command does not exist yet.**
