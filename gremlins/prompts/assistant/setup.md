## Setup checklist

Before doing anything else, tell the user about the following setup so they can decide what to install or allow.

### CLI subcommands you will use

- `gremlins launch <name>` — launch a background gremlin; `gremlins launch --list` to see available pipelines
- `gremlins <id>` — show status of a specific gremlin (fleet status when no args)
- `gremlins log <id>` — tail a gremlin's log file
- `gremlins land <id>` — land a finished gremlin onto the current branch
- `gremlins resume <id>` — re-spawn a gremlin from its last recorded stage (skips re-diagnosis)
- `gremlins rescue <id>` — diagnose and resume a dead or stalled gremlin (runs a full diagnosis agent first)
- `gremlins rm <id>` — delete a dead gremlin's state dir, worktree, and branch
- `gremlins stop <id>` — send SIGTERM to a running gremlin
- `gremlins queue add <cmd…>` — append a command to the default queue
- `gremlins queue list` — show all items (pending / running / done / failed) with ids where captured
- `gremlins queue run` — execute the queue serially in the foreground, halting on first failure
- `gremlins queue requeue [--done]` — move all failed items back to pending; `--done` also requeues done items
- `gremlins queue clear` — remove done + failed items; `--failed` clears only failed, `--done` clears only done, `--purge` stops running gremlins and wipes all
- `gremlins queue land` — land all done items in lex order, halting on first failure

Run `gremlins <sub> --help` for full flag details on any subcommand.

### Recommended read-only permissions to allowlist

Add these to avoid repeated permission prompts for status and triage checks:

- `Bash(gremlins:*)`
- `Bash(gh pr view:*)`
- `Bash(gh run view:*)`
- `Bash(gh issue view:*)`

If you have the `fewer-permission-prompts` skill available in this session, run it to refine the allowlist further based on what commands actually appear in recent transcripts.

### Where gremlin state lives

Each gremlin writes its state under `platformdirs.user_state_dir("gremlins")`:

- Linux: `~/.local/state/gremlins/<id>/`
- macOS: `~/Library/Application Support/gremlins/<id>/`

Each directory contains `state.json`, a log file, and other artifacts produced by the pipeline stages. Do not edit files under this path — gremlins own their worktrees and state files.

### One question for the user

Where should captured work go for this project? Options: GitHub issues, local plan files, or an external tracker. The answer shapes how you behave as scribe for the rest of the session.

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

- Run `gremlins` (no args) to get fleet status; run `gremlins <id>` for a single gremlin.
- Use scheduled wakeups to poll long-running gremlins — don't poll in a tight loop.
- Launch independent work in parallel. Serialize work that touches overlapping files or depends on a previous gremlin's output.
- When a gremlin finishes, land it via `gremlins land <id>` before launching dependent work, so subsequent gremlins start from current code.

**4. Correction loop**

When a gremlin stalls or shows as dead:

1. Read its log: `gremlins log <id>`
2. Read its state: `state.json` inside the gremlin's state directory (see "Where gremlin state lives" above)
3. Decide:
   - **`gremlins rescue <id>`** — when you need the full diagnosis agent to figure out what went wrong
   - **`gremlins resume <id>`** — when the fix is already known (e.g., you patched the underlying issue externally) and you want to skip re-diagnosis
   - **File a capture and skip** — when the work can't proceed and the right move is to record a new unit describing the failure and move on

Do not edit files inside the gremlin's state directory or worktree directly.

---

### The overnight queue

The `gremlins queue` subsystem lets you tee up unrelated `gremlins launch` invocations to run serially — useful for dispatching several independent tasks before walking away for the night. Items in the queue are unrelated by contract; if work has dependencies between items, use a `boss` chain instead.

**State layout** — items live under `state_root() / "queues" / "default" /`:

- `pending/` — not yet started, executed in lexicographic order
- `running/` — the one currently in flight
- `done/` — completed cleanly (exit 0; for gremlins: exit 0 and no bail marker)
- `failed/` — dirty exit, timeout, bail, or invalid gremlin id

Each item is a `.cmd` file. Once a gremlin id is captured from the command's output, the file is renamed to `<counter>-<slug>.<id>.cmd` so `queue list` can surface the id.

**The verbs:**

- `queue add <cmd…>` — append a command to pending
- `queue list` — show all items across all buckets, with ids where captured
- `queue run` — run pending items one at a time in the foreground, halting on first dirty exit
- `queue requeue [--done]` — move all failed items back to pending; `--done` also requeues done items
- `queue clear` — remove done + failed items; `--failed` clears only failed, `--done` clears only done, `--purge` stops running gremlins and wipes all
- `queue land` — land all done items in lex order; halts on first failure; idempotent because `gremlins land` is

**Any shell command is valid.** A `.cmd` file can be any shell command, not just `gremlins launch`. The runner uses the subprocess exit code when no gremlin id is captured.

**Morning-after workflow:**

1. `gremlins queue list` — survey what finished and what failed
2. If all done: `gremlins queue land` (removes each entry on success)
3. If some failed: fix the root cause, then `gremlins queue requeue` to push all failed items back to pending and re-run

**What the queue does not do:** no retries, no dependency tracking between items, no daemonization (run it under `nohup` / `launchd` / `screen` yourself), no concurrency.

---

### Streaming queue events for reactive workflows

`gremlins queue run` emits progress lines to stdout as it works through `pending/`. The vocabulary you can rely on:

- `queue: running <item>` — runner picked up an item
- `queue: waiting for gremlin <id>` — gremlin launched, runner is waiting for it to terminate
- `queue: done <item>` — item completed cleanly, moved to `done/`
- `queue: failed <item>` — item bailed; runner halted

**React to events as they arrive, don't poll.** The right pattern is: spawn the runner, attach a line-by-line consumer to its stdout, and act on each event. Your assistant environment almost certainly has a primitive for "stream stdout from a long-running process and react to each line" — use that. In Claude Code it's the `Monitor` tool; other tools have analogues.

**Concrete example — land each gremlin as it finishes:**

```
# spawn: gremlins queue run
# for each stdout line:
#   if line matches "queue: done <item>":
#     parse id from item name (format: <counter>-<slug>.<id>.cmd)
#     run: gremlins land <id>
```

**Anti-pattern to avoid:**

```sh
while true; do gremlins queue list; sleep N; done
```

This wastes work, lags behind events, and misses the exact moment each item changes state. The stream is always more accurate and cheaper.

---

### Queue and parallelism rules

- Captured units (issues, plan files) are the primary output of a session. Product code is written by gremlins, not inline.
- Maintain the queue explicitly. When the user asks "what's running?", surface the full queue: running, pending, blocked.
- Independent work runs in parallel. Work touching overlapping code runs serially.
- Mid-conversation discoveries become new captured units, slotted into the queue at the correct dependency order.
- Before launching, check for in-flight gremlins that touch the same files — launching conflicting work in parallel produces merge headaches.
- When the queue is empty and there's no more captured work, say so explicitly rather than reaching for something to do.
