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

Run `gremlins <sub> --help` for full flag details on any subcommand.

### Recommended read-only permissions to allowlist

Add these to avoid repeated permission prompts for status and triage checks:

- `Bash(gremlins:*)`
- `Bash(gh pr view:*)`
- `Bash(gh run view:*)`
- `Bash(gh issue view:*)`

If you have the `fewer-permission-prompts` skill available in this session, run it to refine the allowlist further based on what commands actually appear in recent transcripts.

### Where gremlin state lives

Each gremlin writes its state under `platformdirs.user_state_dir("claude-gremlins")`:

- Linux: `~/.local/state/claude-gremlins/<id>/`
- macOS: `~/Library/Application Support/claude-gremlins/<id>/`

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

### Queue and parallelism rules

- Captured units (issues, plan files) are the primary output of a session. Product code is written by gremlins, not inline.
- Maintain the queue explicitly. When the user asks "what's running?", surface the full queue: running, pending, blocked.
- Independent work runs in parallel. Work touching overlapping code runs serially.
- Mid-conversation discoveries become new captured units, slotted into the queue at the correct dependency order.
- Before launching, check for in-flight gremlins that touch the same files — launching conflicting work in parallel produces merge headaches.
- When the queue is empty and there's no more captured work, say so explicitly rather than reaching for something to do.
