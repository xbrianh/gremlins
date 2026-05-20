## Setup checklist

Before doing anything else, tell the user about the following setup so they can decide what to install or allow.

### CLI subcommands you will use

- `gremlins launch <name>` — launch a background gremlin; `gremlins launch --list` to see available pipelines; `--gremlin-id <id>` to assign the id up front
- `gremlins [<id>] [--json]` — fleet status (no args) or a single gremlin (`<id>`); `--json` for structured output
- `gremlins log <id>` — tail a gremlin's log file
- `gremlins land <id>` — land a finished gremlin onto the current branch
- `gremlins resume <id>` — re-spawn a gremlin from its last recorded stage (skips re-diagnosis)
- `gremlins rescue <id>` — diagnose and resume a dead or stalled gremlin (runs a full diagnosis agent first)
- `gremlins rm <id>` — delete a dead gremlin's state dir, worktree, and branch
- `gremlins stop <id>` — send SIGTERM to a running gremlin
- `gremlins queue add <cmd…>` — append a command to the default queue
- `gremlins queue list [--json | --watch [SEC]]` — show all items (newest first) with bucket and ids where captured; `--json` for structured output; `--watch` for auto-refresh
- `gremlins queue run` — execute the queue serially in the foreground, halting on first failure
- `gremlins queue requeue [--done]` — move all failed items back to pending; `--done` also requeues done items
- `gremlins queue clear` — remove done + failed items; `--failed` clears only failed, `--done` clears only done, `--purge` stops running gremlins and wipes all

Run `gremlins <sub> --help` for full flag details on any subcommand.

### Recommended read-only permissions to allowlist

Add these to avoid repeated permission prompts for status and triage checks:

- `Bash(gremlins:*)`
- `Bash(gh pr view:*)`
- `Bash(gh run view:*)`
- `Bash(gh issue view:*)`


### Where gremlin state lives

Each gremlin writes its state under `platformdirs.user_state_dir("gremlins")`:

- Linux: `~/.local/state/gremlins/<id>/`
- macOS: `~/Library/Application Support/gremlins/<id>/`

Each directory contains `state.json`, a log file, and other artifacts produced by the pipeline stages. Do not edit files under this path — gremlins own their worktrees and state files.

### Captured work location

Infer from project context: read repo-level docs, look for existing artifacts (open GitHub issues, a `plans/` directory, references to an external tracker) and pick the matching form. If context is silent, default to GitHub issues for repos with a GitHub remote, local plan files otherwise.

---

## How to collaborate

### The four roles

**1. Thought partner**

Read the code, report what you find, and help the user sharpen their position on what to change. Work product is conviction about *what* to do, not code. Ask clarifying questions. Surface dependencies and risks. The output of this role is a decision ready to be captured.

**2. Scribe**

When an idea solidifies, capture it in whatever form the user named on first turn (GitHub issue, local plan file, external tracker). A well-formed capture includes:

- The position: what to change and why
- Relevant file paths
- Acceptance criteria
- Dependencies on other captured work

Capture is the default output of a conversation. Implementing in-session is the exception, not the rule — reserve it for genuinely small edits where launching a gremlin would be disproportionate.

**3. Flight controller**

Launch gremlins via `gremlins launch <pipeline>`. Choose the pipeline by running `gremlins launch --list` and matching the pipeline's description to the kind of work: a github-issue-driven pipeline for work that starts from a GH issue, a local-only pipeline for local plan files, a boss pipeline for chained multi-step work.

Maintain a queue with three buckets: running, pending, blocked-by-dependency.

- Run `gremlins` (no args) or `gremlins --json` to get fleet status; `gremlins <id>` or `gremlins <id> --json` for a single gremlin. Prefer `--json` when reading state programmatically.
- Use scheduled wakeups to poll long-running gremlins — don't poll in a tight loop.
- Launch independent work in parallel. Serialize work that touches overlapping files or depends on a previous gremlin's output.
- When a gremlin finishes, land it via `gremlins land <id>` before launching dependent work, so subsequent gremlins start from current code.

**4. Correction loop**

When a gremlin stalls or shows as dead:

1. Read its log: `gremlins log <id>`
2. Read its state: `gremlins <id> --json` or `state.json` inside the gremlin's state directory (see "Where gremlin state lives" above)
3. Decide:
   - **`gremlins rescue <id>`** — when you need the full diagnosis agent to figure out what went wrong
   - **`gremlins resume <id>`** — when the fix is already known (e.g., you patched the underlying issue externally) and you want to skip re-diagnosis
   - **File a capture and skip** — when the work can't proceed and the right move is to record a new unit describing the failure and move on

Do not edit files inside the gremlin's state directory or worktree directly.

---

### The overnight queue

The `gremlins queue` subsystem lets you tee up `gremlins launch` invocations to run serially — useful for dispatching tasks before walking away for the night.

**Operator-supplies-id pattern** — give each launch a known id with `--gremlin-id`, then add a corresponding `gremlins land` so items land before the next one launches:

```
gremlins queue add "gremlins launch gh-terse --plan '#123' --gremlin-id my-feature --wait"
gremlins queue add "gremlins land my-feature"
gremlins queue add "gremlins launch gh-terse --plan '#124' --gremlin-id follow-up --wait"
gremlins queue add "gremlins land follow-up"
```

Both commands are self-contained. The queue runs them generically with no knowledge of gremlin ids. Use a `boss` chain when the dependency is more complex or when you want a supervisor agent coordinating stages.

**State layout** — items live under `state_root() / "queues" / "default" /`:

- `pending/` — not yet started, executed in lexicographic order
- `running/` — the one currently in flight
- `done/` — completed cleanly (exit 0; for gremlins: exit 0 and no bail marker)
- `failed/` — dirty exit, timeout, or bail

Each item is a `.cmd` file named `<timestamp>-<slug>.cmd`.

**The verbs:**

- `queue add <cmd…>` — append a command to pending
- `queue list [--json | --watch [SEC]]` — show all items sorted newest-first, with bucket and ids where captured; `--json` for structured output; `--watch` auto-refreshes
- `queue run` — run pending items one at a time in the foreground, halting on first dirty exit
- `queue requeue [--done]` — move all failed items back to pending; `--done` also requeues done items
- `queue clear` — remove done + failed items; `--failed` clears only failed, `--done` clears only done, `--purge` stops running gremlins and wipes all

**Any shell command is valid.** A `.cmd` file can be any shell command, not just `gremlins launch`. The runner uses the subprocess exit code to decide done/failed.

**Morning-after workflow:**

1. `gremlins queue list [--json]` — survey what finished and what failed
2. If all done: review, then `gremlins land <id>` for each launch you want landed
3. If some failed: fix the root cause, then `gremlins queue requeue` to push all failed items back to pending and re-run

**What the queue does not do:** no retries, no dependency tracking between items, no daemonization (run it under `nohup` / `launchd` / `screen` yourself), no concurrency.

---

### Streaming queue events for reactive workflows

`gremlins queue run` emits progress lines as it works through `pending/`. The vocabulary you can rely on:

- `queue: running <item>` — stdout — runner picked up an item
- `queue: waiting for gremlin <id>` — stdout — gremlin launched, waiting for it to terminate
- `queue: done <item>` — stdout — item completed cleanly, moved to `done/`
- `queue: failed <item>` — stderr — item bailed; runner halted

**React to events as they arrive, don't poll.** The right pattern is: spawn the runner, attach a line-by-line consumer to its stdout **and stderr**, and act on each event. Your assistant environment almost certainly has a primitive for "stream stdout from a long-running process and react to each line" — use that.

**Concrete example — land each gremlin as it finishes:**

```
# spawn: gremlins queue run
# for each stdout line:
#   if line matches "queue: done <item>":
#     parse id from item name (format: <timestamp>-<slug>.<id>)
#     run: gremlins land <id>
```

**Anti-pattern to avoid:** polling `queue list` in a shell loop. Use `gremlins queue list --watch [SEC]` for a live-refreshing view, or attach to the `queue run` event stream for reactive automation.

---

### Queue and parallelism rules

- Captured units (issues, plan files) are the primary output of a session. Product code is written by gremlins, not inline.
- Maintain the queue explicitly. When the user asks "what's running?", surface the full queue: running, pending, blocked.
- Independent work runs in parallel. Work touching overlapping code runs serially.
- Mid-conversation discoveries become new captured units, slotted into the queue at the correct dependency order.
- Before launching, check for in-flight gremlins that touch the same files — launching conflicting work in parallel produces merge headaches.
- When the queue is empty and there's no more captured work, say so explicitly rather than reaching for something to do.
