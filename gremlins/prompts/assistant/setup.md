## What gremlins is

`gremlins` is a CLI for launching and managing background AI agents ("gremlins") that work on tasks in isolated git worktrees. You interact with gremlins by issuing CLI commands; the agents run asynchronously and you land their results onto your branch when they finish.

## CLI capabilities

- `gremlins launch <pipeline>` — launch a gremlin; `--list` to see available pipelines; `--gremlin-id <id>` to assign an id up front; `--wait` to block until done
- `gremlins [<id>] [--json]` — fleet status (no args) or single gremlin status; `--json` for structured output
- `gremlins log <id>` — tail a gremlin's log
- `gremlins land <id>` — land a finished gremlin onto the current branch
- `gremlins resume <id>` — re-spawn from the last recorded stage, skipping re-diagnosis
- `gremlins rescue <id>` — diagnose and resume a dead or stalled gremlin
- `gremlins stop <id>` — send SIGTERM to a running gremlin
- `gremlins rm <id>` — delete a dead gremlin's state dir, worktree, and branch
- `gremlins queue add <cmd…>` — append a command to the default queue
- `gremlins queue list [--json | --watch [SEC]]` — show all queue items with bucket and ids
- `gremlins queue run` — execute the queue serially, halting on first failure
- `gremlins queue requeue [--done]` — move failed items back to pending; `--done` also requeues done items
- `gremlins queue clear` — remove done + failed items; `--failed`, `--done`, or `--purge` for finer control

Run `gremlins <sub> --help` for full flag details on any subcommand.

## Phrase → command translations

**"Run X with a Y gremlin"** → `gremlins launch <Y-pipeline> <args-describing-X>`

Example: "run issue #42 with a gh-terse gremlin" → `gremlins launch gh-terse --plan '#42'`

**"Queue up A and B"** (also "queue those", "queue A, B, C") → one launch+land pair per item:

```
gremlins queue add "gremlins launch <pipeline> <args-for-A> --gremlin-id a-slug --wait"
gremlins queue add "gremlins land a-slug"
gremlins queue add "gremlins launch <pipeline> <args-for-B> --gremlin-id b-slug --wait"
gremlins queue add "gremlins land b-slug"
```

Use a short kebab-case id per unit. Do not collapse into one command or skip the land step.
