# gremlins — top-level AGENTS

Background orchestration for coding agents: a gremlin is a detached process that runs a YAML-defined pipeline (plan → implement → review → address → open-PR …) against a goal or GitHub issue, writing artifacts to a per-user state directory.

Gremlins is an **unopinionated agentic workflow language**: the pipeline YAML is the program, and the harness is only its runtime. The harness keeps injected opinion to a minimum — a thin system prompt carries tool definitions, directory layout, and (for now) some pragmatic guidance like delegation policy. Major behavioral opinions (what to re-check, how to communicate, when to bail) belong in the pipeline's own prompt files, never in harness code or a bundled default.

> **Note:** model-specific guidance will need to live in the harness system prompt in the future. The current opinionated bits (e.g. `<important>` delegation block) are a temporary compromise; the boundary will firm up as we learn what each model needs.

This file is the entry-point orientation for an agent working on this codebase. The user-facing project doc is `README.md`. Design notes live in `DESIGN.md` and `plans/`.

## Repository layout

```
Cargo.toml                   Rust workspace root
crates/
  gremlins/                  The main crate — executor, stages, clients, artifacts, CLI
    src/
      lib.rs              Crate root
      config.rs              Configuration loading
      clients/              Client backends + agent loop — see src/clients/
      stages/                Stage types: agent, exec, loop, composite, parallel, sequence
      artifacts/            Artifact registry + URI model
      core/                 Core utilities: git, proc, discovery, yaml_io, env_file
      schemas/              Pipeline schema types
.gremlins/                   Project-overlay pipeline YAMLs (project-scoped)
  bin/                      Shell scripts used by pipelines
  prompts/                  Bundled prompt templates
  stages/                   Custom stage YAMLs
plans/                       Design notes, in-flights plan documents, per-feature sketches
DESIGN.md                    System design
README.md                   Dev install + CLI usage
```

## Dev workflow

```sh
cargo build              # debug build
cargo test -p gremlins   # run Rust tests
makecheck               # fmt-check + clippy + cargo check
maketest                # cargo test -q -p gremlins --lib
```

The `Makefile` sets `MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)` automatically.

## Project-wide conventions

- **Unopinionated workflow language.** The harness supplies mechanics (sequencing, worktrees, artifacts, bail bookkeeping, client plumbing) and keeps injected opinion to a minimum. A thin harness system prompt carries tool definitions, directory layout, and pragmatic guidance (e.g. delegation policy). Major behavioral instructions still belong in the pipeline's own prompt files where the pipeline author owns them. Model-specific guidances will live in the harness system prompt as a long-term necessity; for now, some opinionated bits are in the harness as a temporary compromise.
- **No inheritance.** Composition only. Single inheritance is almost always the wrong tool; multiple inheritance is never acceptable.
- **Short functions.** If it doesn't fit on a screen, split it.
- **Few comments.** Names carry meaning. Comment only when *why* is non-obious.
- **Worktree invariant (in-progress, see #395):** gremlin worktrees should operate on detached HEAD throughout the run; commits accumulate on detached HEAD; the PR-opening primitive pushes a remote branch directly. Existing code is mid-retrofit.

## Byte-stable strings — DO NOT change

These values are persisted to `state.json` and read by other writers (the fleet manager, the launcher). Renaming any of them silently breaks cross-process consumers.

- **Bail classes** (`state.json.bail_class`): `reviewer_requested_changes`, `security`, `secrets`, `other`. These are convention tokens parsed by the agent stage from the `BAIL: <class>: <detail>` marker (`crates/gremlins/src/stages/agent.rs`).
- **Stage names** (`state.json.stage`): defined per-pipeline in YAML. The authoritative list for a pipeline is its YAML file under `.gremlins/`.

## Where to look for…

| You want to … | Look at |
|---|---|
| Understand the run-time architecture | `DESIGN.md` |
| Add a new stage | `crates/gremlins/src/stages/` and an existing stage as a model |
| Add a new client provider | `crates/gremlins/src/clients/` |
| Add a new pipeline | YAMLs in `.gremlins/` |
| Find the design backlog | `plans/` (rough notes, not authoritative) |
| Find open work | GitHub issues, `gh issue list --repo xbrianh/gremlins` |

## State and bail bookkeeping

`State::set_stage` writes stage info to `state.json` atomically via `State::patch`.
`State::write_bail_file` writes `bail_{attempt}.json` to the state dir. When a stage
detects a recorded bail (via `state.json`), it returns a bail error.
Both helpers no-op without `GREMLINS_GREMLIN_ID` and never panic —
stage / bail bookkeeping must not crash a running gremlin.