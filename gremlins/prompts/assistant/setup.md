## Install the gremlins skills (recommended)

For any assistant that supports slash-command skills (Claude Code is the reference target), the best experience is to install `/gremlins-launch` and `/gremlins-queue` rather than driving the CLI directly. The skills wrap the CLI with the right guardrails; raw CLI use still works and is documented below for assistants without skill support.

### How to create each skill

Create a `SKILL.md` file under your assistant's skills directory. For Claude Code that is `~/.claude/skills/<skill-name>/SKILL.md`. For other assistants, use wherever that assistant loads skills.

**`/gremlins-launch`** — launch a single background gremlin by pipeline name when the user names one unit of work to run now. Not for queues, chains, or scheduled work.

Steps to create it:
1. Create `~/.claude/skills/gremlins-launch/SKILL.md`.
2. Add frontmatter: `name: gremlins-launch`, `description: Launch a single background gremlin by pipeline name`, `argument-hint: <pipeline> [args]`.
3. Populate the body from `gremlins launch --help` output. Do **not** hardcode pipeline names — pipeline names are project-specific and change as operators add or modify pipelines under `.gremlins/` in the project. The skill must call `gremlins launch --list` at use time to discover available pipelines, and `gremlins launch <pipeline> --help` to learn each pipeline's flags.

**`/gremlins-queue`** — run one long-lived `gremlins queue run` in the background and append paired `launch --wait` + `land` commands as the user names units of work. Default pipeline `gh-terse` when the user is working with GitHub issues, unless they name another.

Steps to create it:
1. Create `~/.claude/skills/gremlins-queue/SKILL.md`.
2. Add frontmatter: `name: gremlins-queue`, `description: Queue gremlin work with a persistent background runner`, `argument-hint: [pipeline] work-description`.
3. Populate the body from `gremlins queue --help`, `gremlins queue run --help`, and `gremlins queue add --help` output. Bake in these invariants:
   - **One runner per session.** Start `gremlins queue run` in the background once; never re-spawn it.
   - **One unit = one launch+land pair.** Each unit of work is exactly one `gremlins launch <pipeline> <args> --gremlin-id <id> --wait` followed by one `gremlins land <id>`. Never collapse two units into one command or skip the land step.
   - **Assistant-generated ids.** The assistant generates a short kebab-case `--gremlin-id` and passes the same id to both the `launch` and `land` commands.
   - **No scope expansion.** Queue exactly what the user named — not that plus anything else outstanding.

---

## What gremlins is

`gremlins` is a CLI for launching and managing background AI agents ("gremlins") that work on tasks in isolated git worktrees. You interact with gremlins by issuing CLI commands; the agents run asynchronously and you land their results onto your branch when they finish.

## CLI capabilities

- `gremlins launch <pipeline>` — launch a gremlin; `--list` to see available pipelines; `--gremlin-id <id>` to assign an id up front; `--wait` to block until done; pipeline-specific flags (e.g. `--plan <spec>`) follow `<pipeline>`
- `gremlins [<id-prefix>] [--json]` — fleet status (no args) or single gremlin status; `--json` for structured output
- `gremlins log <id-prefix>` — tail a gremlin's log
- `gremlins land <id-prefix>` — land a finished gremlin onto the current branch
- `gremlins resume <id-prefix>` — re-spawn from the last recorded stage, skipping re-diagnosis
- `gremlins rescue <id-prefix>` — diagnose and resume a dead or stalled gremlin
- `gremlins stop <id-prefix>` — send SIGTERM to a running gremlin
- `gremlins rm <id-prefix>` — delete a dead gremlin's state dir, worktree, and branch
- `gremlins queue add <cmd…>` — append a command to the default queue
- `gremlins queue list [--json | --watch [SEC]]` — show all queue items with bucket and status
- `gremlins queue run` — execute the queue serially, halting on first failure; watches for new items when empty (use `--once` to exit instead of watching; `--poll-interval SEC` to tune the polling interval, default 1s)
- `gremlins queue requeue [--done]` — move failed items back to pending; `--done` also requeues done items
- `gremlins queue clear` — remove done + failed items; `--failed`, `--done`, or `--purge` for finer control

Run `gremlins <sub> --help` for full flag details on any subcommand.

## Phrase → command translations

**"Run X with a Y gremlin"** → `gremlins launch <Y-pipeline> <args-describing-X>`

Example: "run issue #42 with a gh gremlin" → `gremlins launch gh --plan '#42'`

**"Queue up A and B"** (also "queue those", "queue A, B, C") → one launch+land pair per item:

```
gremlins queue add "gremlins launch <pipeline> <args-for-A> --gremlin-id a-slug --wait"
gremlins queue add "gremlins land a-slug"
gremlins queue add "gremlins launch <pipeline> <args-for-B> --gremlin-id b-slug --wait"
gremlins queue add "gremlins land b-slug"
```

Use a short kebab-case id per unit. Do not collapse into one command or skip the land step.
