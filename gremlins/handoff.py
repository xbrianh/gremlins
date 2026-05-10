#!/usr/bin/env python3
"""handoff.py — per-step planner for chained gremlin workflows.

Reads the current plan document, inspects the diff accumulated on the branch
since the chain started, decides whether there is more work to do, and writes
an updated plan document plus (on next-plan) a child plan suitable for
`launch.sh --plan`.

Exit codes:
  0  — one of the three recognized outcomes; read signal file to distinguish
  1  — infrastructure failure (bad args, missing claude, agent crash, etc.)
"""

import argparse
import json
import logging
import pathlib
import re
import shutil
import sys
import threading
from collections.abc import Callable
from typing import Any, NoReturn, TypeVar, cast

from gremlins.clients import ClaudeClient, PACKAGE_DEFAULT, ClientSpec, to_client
from gremlins.logging_setup import configure_logging
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.utils import proc

logger = logging.getLogger(__name__)

CLAUDE_SANITIZE_MODEL = "haiku"

T = TypeVar("T")


def sanitize_model_for(client_spec: ClientSpec) -> str:
    return (
        CLAUDE_SANITIZE_MODEL if client_spec.provider == "claude" else client_spec.model
    )


def with_reap_after(
    client: ClaudeClient, timeout: int | None, fn: Callable[[], T]
) -> T:
    """Run fn, reaping the client's subprocesses if it doesn't return in time.

    A reap unblocks a stalled client.run by terminating the underlying process,
    which then surfaces as a RuntimeError to the caller.
    """
    if timeout is None:
        return fn()
    timer = threading.Timer(timeout, client.reap_all)
    timer.daemon = True
    timer.start()
    try:
        return fn()
    finally:
        timer.cancel()


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _load_handoff_style() -> str:
    path = BUNDLED_PROMPT_DIR / "code_style.md"
    if not path.exists():
        die(f"error loading prompt: prompt file not found: {path}")
    text = path.read_text(encoding="utf-8").rstrip()
    if not text.strip():
        die(f"error loading prompt: prompt file is empty: {path}")
    return text


def auto_name_out(plan_path: pathlib.Path) -> pathlib.Path:
    """Given plan.md → plan-001.md; given plan-001.md → plan-002.md, etc."""
    # Strip trailing -NNN so plan-001.md produces plan-002.md, not plan-001-001.md
    base = re.sub(r"-\d{3}$", "", plan_path.stem) or plan_path.stem
    parent = plan_path.parent
    n = 1
    while True:
        candidate = parent / f"{base}-{n:03d}.md"
        if not candidate.exists():
            return candidate
        n += 1


def collect_git_context(
    base_ref: str | None, rev: str | None = None
) -> tuple[str, str, str]:
    """Return (branch_name, git_log, git_diff) since merge-base with base_ref.

    If rev is given, inspect that ref instead of HEAD (useful when the caller
    cannot check out the target branch, e.g. the boss gremlin inspecting the
    target branch tip from its frozen worktree context).
    """
    target = base_ref or "main"
    inspect_rev = rev or "HEAD"

    # Validate target ref exists early so we get a clear error message
    result = proc.run(["git", "rev-parse", "--verify", target])
    if result.returncode != 0:
        die(f"--base ref not found in repo: {target!r}")

    if rev is not None:
        result = proc.run(["git", "rev-parse", "--verify", rev])
        if result.returncode != 0:
            die(f"--rev ref not found in repo: {rev!r}")

    result = proc.run(["git", "rev-parse", "--abbrev-ref", inspect_rev])
    branch = result.stdout.strip() if result.returncode == 0 else inspect_rev
    if branch == "HEAD":
        # Detached HEAD: surface the SHA so the prompt doesn't read "Branch: HEAD".
        sha = proc.run(["git", "rev-parse", inspect_rev]).stdout.strip()
        branch = f"(detached at {sha[:12]})" if sha else "(detached)"

    result = proc.run(["git", "merge-base", inspect_rev, target])
    if result.returncode != 0:
        die(f"could not compute merge-base between {inspect_rev!r} and {target!r}")
    merge_base = result.stdout.strip()

    result = proc.run(
        ["git", "log", f"{merge_base}..{inspect_rev}", "--oneline"], check=True
    )
    git_log = result.stdout.strip()

    result = proc.run(["git", "diff", f"{merge_base}..{inspect_rev}"], check=True)
    git_diff = result.stdout

    return branch, git_log, git_diff


def build_prompt(
    plan_text: str,
    branch: str,
    git_log: str,
    git_diff: str,
    out_path: pathlib.Path,
    child_plan_path: pathlib.Path,
    signal_path: pathlib.Path,
    spec_text: str | None = None,
) -> str:
    diff_body = git_diff[:50000] if git_diff else "(empty — no changes yet)"
    diff_trunc = (
        f"\n(diff truncated to 50000 chars; {len(git_diff)} chars total)"
        if len(git_diff) > 50000
        else ""
    )
    log_body = git_log if git_log else "(no commits yet — branch just started)"

    spec_section = ""
    if spec_text is not None:
        spec_body = spec_text[:50000]
        spec_trunc = (
            f"\n(spec truncated to 50000 chars; {len(spec_text)} chars total)"
            if len(spec_text) > 50000
            else ""
        )
        spec_section = f"""## Overarching goal (north star)

This is the original chain spec. It does not change between handoffs and is read-only context for understanding what the chain as a whole is working toward. Use it to judge whether the rolling input plan below is on track and to scope the next step coherently. Do not echo it into the updated plan — it stays in `--spec`.

~~~~
{spec_body}
~~~~{spec_trunc}

"""

    code_style = _load_handoff_style()
    style_section = f"""## Coding style

Respect these principles when writing child plans. Avoid proposing architectures that violate them — e.g. multi-level class hierarchies, factories where a function suffices, speculative abstractions:

{code_style}

"""

    return f"""You are a chain-manager agent. Inspect the plan document and the work that has landed on the current branch, then decide whether the chain is complete or a next step is needed.

{spec_section}{style_section}## Input plan

~~~~
{plan_text}
~~~~

## Branch context

Branch: {branch}

Git log since chain start:
```
{log_body}
```

Git diff since chain start:
```diff
{diff_body}
```
{diff_trunc}

## Implementation vs operator boundary

A child gremlin operates **inside a detached-HEAD worktree, against a single feature branch, ending in one squash-merged PR**. Anything that requires being outside that scope — the user's live config, another worktree, multiple branches, post-merge actions, sibling gremlin launches — is an **operator task**, owned by the human between phase landings.

Classify a task as **operator** if executing it inside a child gremlin's worktree would be impossible, destructive, or undefined. Concrete signals:

- **Mutates the user's live config or other shared machine state directly**: hand-edits under `~/.claude/` (or equivalent live config dirs), running a script that mirrors the worktree into shared state, copying built artifacts onto the user's machine. (The child has unmerged code in its worktree; pushing that into live state would suddenly run unmerged code on the user's machine.)
- **Launches another gremlin**: `/localgremlin`, `/ghgremlin`, `/bossgremlin`, or a smoke-run / end-to-end run that boils down to invoking one. Recursive gremlin launch from a detached worktree is undefined behavior.
- **Pushes to a remote outside the PR flow**: `git push origin main`, force-pushes, manual `gh pr merge`, direct merges. The child's only remote interaction is opening (and updating) one PR.
- **Operator commands**: `/gremlins land`, `/gremlins rescue`, `/gremlins stop`, `/gremlins close`, `/gremlins rm`. These are human controls, not workflow steps.
- **Post-merge verification**: "verify the merged PR's CI is green", "confirm the production deploy", "watch the release dashboard". The child finishes before its PR merges.

Classify as **implementation** if it is a code/doc/config change that lands in the child's PR. Examples that *look* operator-adjacent but are implementation:

- "Update a tracked docs file to describe a new module" — edits a tracked repo file. The fact that the file may later be mirrored to live machine state by an operator step is irrelevant: the child edits the in-repo copy, the human runs the mirror later.
- "Extend a tracked configuration script to handle a new case" — edits a tracked script. Implementation.
- "Add a new design or docs file under the repo" — creates a tracked file. Implementation.
- "Run a tooling dry-run against the user's live config and confirm output is clean" — operator. The dry-run reads live machine state and isn't a code change. (A dry-run *check encoded as a unit test* against fixture data would be implementation; a real invocation against the user's live tree is not.)

The distinction is **what the task changes** (tracked repo files = implementation) vs **what the task reads or mutates outside the worktree** (live user config, sibling processes, remotes outside the PR = operator). When in doubt, ask: "Could a fresh gremlin with no access to my home directory do this?" If no, operator.

If the spec author wrote operator-flavoured language inline with implementation work, **rewrite or drop it; do not copy it verbatim into the child plan**. Operator tasks land only in the rolling plan's `## Operator follow-ups` section, where the human operator picks them up between phase landings.

## Sizing the next step

Prefer **smaller, single-purpose** child plans over bundled ones. A good child plan produces a PR a human reviewer can hold in their head — roughly one focused concern, not a grab bag of "while we're here" changes. Concretely:

- If the remaining `## Tasks` span multiple distinct concerns (e.g. a refactor *and* a new feature, or two unrelated subsystems), pick **one** for this child plan and leave the rest in the rolling plan for a later handoff. Do not collapse them into one child just because they share a theme.
- When a single plan task is itself large or has natural sub-phases (scaffolding → wiring → migration → cleanup), split it: include only the next coherent slice in the child plan, and rewrite the rolling plan's task entry to reflect what remains.
- Err on the side of "one more handoff" rather than one oversized PR. The chain is cheap; large diffs are expensive to review and risky to land.
- Don't go pathologically small either — a child plan should still be a meaningful unit of work, not a single-line tweak. The target is "one reviewable PR", not "one commit".

## Your task

1. Read the plan. Identify every task listed under `## Tasks`, plus every pending entry under `## Operator follow-ups` if the input plan has that section (a previous handoff may have written it). Both sets feed step 3's classification.
2. Compare each `## Tasks` entry against the landed diff and git log to determine whether it has been implemented. Operator follow-ups generally leave no signal in the worktree's diff (they happen outside the worktree by design), so do not infer their completion from git history.
3. Classify every still-open task as **implementation** or **operator** using the boundary above. Operator tasks never land in a child plan.
4. Decide the exit state:
   - **`chain-done`**: all *implementation* tasks in the plan are implemented and landed. Operator tasks do **not** block `chain-done` — they are surfaced separately for the human operator via the `operator_followups` field in the signal file (and the `## Operator follow-ups` section in the rolling plan, if any pending). A chain whose remaining work is operator-only therefore exits as `chain-done`.
   - **`next-plan`**: at least one *implementation* task remains; the next gremlin should tackle it.
   - **`bail`**: something prevents safe continuation (broken state, incoherent plan, security issue, etc.). Reserved for genuine blockers. Operator-only remaining work is **not** a bail reason — it is `chain-done`.

5. Write an **updated plan document** (the "rolling plan") to: `{out_path}`

   The rolling plan describes only **remaining** work. These forms are **never** allowed anywhere in the document, at any position:
   - Prose statements about what has landed, shipped, merged, or been completed — e.g. "Phases 0–3 have landed", "X was merged in PR #N", "the following work is complete", "all tasks in this phase are done"
   - Bullet lists enumerating completed phases or items
   - `[x]` checkboxes or checked markers of any kind
   - Struck-through entries (~~text~~)
   - An H1 title (`# ...`) that names the overall chain goal or summarizes the completed chain — scope the H1 to the remaining work only; e.g. use `# Add sanitize pass` not `# Implement Full Feature X`

   The chain of versioned plan files plus git history is the audit trail; the rolling plan does not repeat it. Do not propagate the overarching goal of the chain forward into the rolling plan — that lives upstream, in the original spec.

   - **`next-plan`**: include only the implementation tasks that are not yet implemented (still `[ ]`). Prune the surrounding sections (`## Context`, `## Approach`, `## Open questions`, etc.) to match: drop sections whose reason for existing was a now-completed task; keep or trim the rest so the document stays a coherent description of the remaining work.
     - Under `## Open questions`, carry forward unresolved entries; drop entries tied to completed tasks.
     - If a task is only partly landed, keep it (rewritten if needed to reflect what remains).
     - Add an `## Operator follow-ups` section listing every pending operator task. Treat the input plan's `## Operator follow-ups` section as authoritative for prior follow-ups: carry forward every item that still appears there. Only drop an entry if the input plan or git history makes its completion unambiguous (e.g. the human/operator removed it from the input plan, or a commit message explicitly states the operator step was done). Do **not** infer completion from git diff/log or implementation progress alone — operator tasks happen outside the worktree, so the safe behavior is conservative carry-forward. Add any new operator-classified items found in this pass alongside the carried-forward entries. If after all that there are no pending operator tasks, omit the section.
   - **`chain-done`**: minimal output. A short note that the chain is complete is enough — no leftover task list, no carried-over context. If any pending operator follow-ups remain (under the carry-forward rule above), list them under `## Operator follow-ups` so the human sees them in the final rolling plan; otherwise omit. The signal file carries the structured outcome (including `operator_followups`).
   - **`bail`**: same pruning rules as `next-plan` (only remaining implementation tasks, surrounding sections trimmed accordingly, unresolved `## Open questions` carried forward, `## Operator follow-ups` carried forward under the conservative rule above), with a bail-reason banner added prominently at the top.

6. If exit state is **`next-plan`**, write a **child plan** to: `{child_plan_path}`
   - Use the standard localgremlin plan structure exactly:

     ```
     # <short one-line title summarising what this step implements>

     ## Context
     <brief description of what this child gremlin should implement>

     ## Approach
     <implementation approach for the remaining work>

     ## Tasks
     - [ ] Task N: ...
     <only the implementation tasks that are not yet done — never operator tasks>

     ## Open questions
     <risks or open questions, or "(none)" if there are none>
     ```
   - The child plan must be self-contained — a fresh gremlin with only this file must know exactly what to implement. Do not propagate the overarching goal of the chain into the child plan; scope it to the next chunk per the **Sizing the next step** rules above. If you find yourself listing tasks that span multiple concerns or natural phases, stop and narrow the scope — push the rest back into the rolling plan for the next handoff.
   - **No operator tasks in the child plan, ever.** Before writing the child plan, re-read your own draft `## Tasks` list and ask, for each item: "Is this something a code-only gremlin in a detached worktree can do, ending in one PR?" If any task fails that test, revise — rewrite it as the underlying code change if there is one, or move it to `## Operator follow-ups` in the rolling plan and drop it from the child plan.

7. Write the **signal marker** to: `{signal_path}`
   - Valid JSON, exactly this structure:
     ```json
     {{"exit_state": "next-plan|chain-done|bail", "child_plan": "<absolute path or null>", "reason": "<bail reason or null>", "operator_followups": ["<task>", ...]}}
     ```
   - `child_plan`: `{child_plan_path}` (as a string) if exit state is `next-plan`, otherwise `null`.
   - `reason`: a short human-readable explanation if exit state is `bail`, otherwise `null`.
   - `operator_followups`: an array of one-line strings describing every pending operator task, mirroring the rolling plan's `## Operator follow-ups` section. Empty array `[]` if there are none. Required on every exit state — including `chain-done`, where this is how the boss orchestrator learns about operator tasks the human still owes after the rolling plan has been pruned to a "chain complete" note.

Write all required files before finishing. Do not explain your reasoning in stdout — the files are the output."""


def build_sanitize_prompt(rolling_plan_text: str, out_path: pathlib.Path) -> str:
    return f"""You are a format-enforcement agent. Rewrite the rolling plan below to remove every violation of the rules listed here, then write ONLY the rewritten document to: {out_path}

## Rules — these patterns are NEVER allowed anywhere in the document

1. Prose statements about what has landed, shipped, merged, or been completed at any document position — e.g. "Phases 0–3 have landed", "X was merged in PR #N", "the following work is complete", "all tasks in this phase are done". Remove such sentences entirely.
2. Bullet lists enumerating completed phases or items — any bullet that describes something already done. Remove them.
3. `[x]` checkboxes or checked markers of any kind. Remove the entire line.
4. Struck-through entries (~~text~~). Remove the entire line.
5. An H1 title (`# ...`) that names the overall chain goal or summarizes the completed chain — e.g. `# Implement Feature X` or `# Claude Config Personal Setup`. Replace it with a short H1 scoped only to the remaining work.

## What to keep

Keep all remaining task lists (`- [ ] ...`), open questions, context relevant to what is still to be done, and operator follow-ups. You may make minimal wording changes only when needed to satisfy the rules above, such as replacing a too-broad H1 with one scoped to the remaining work or rephrasing surrounding context so it refers only to unfinished work. Do not invent new tasks, requirements, decisions, or factual claims that are not supported by the original.

## Output

Write ONLY the rewritten document to: {out_path}
Do not print the document to stdout. Do not explain what you changed.

## Rolling plan to rewrite

~~~~
{rolling_plan_text}
~~~~"""


def _restore_rolling_plan(
    out_path: pathlib.Path, original_text: str, reason: str
) -> None:
    try:
        out_path.write_text(original_text, encoding="utf-8")
    except OSError as exc:
        sys.stderr.write(
            f"warning: {reason} — failed to restore original rolling plan: {exc}\n"
        )
        return
    sys.stderr.write(f"warning: {reason} — restored original rolling plan\n")


def _read_rolling_plan_for_sanitize(out_path: pathlib.Path) -> str | None:
    if not out_path.exists():
        sys.stderr.write(
            f"warning: sanitize skipped — rolling plan not found: {out_path}\n"
        )
        return None
    try:
        return out_path.read_text(encoding="utf-8")
    except OSError as exc:
        sys.stderr.write(
            f"warning: sanitize skipped — could not read rolling plan: {exc}\n"
        )
        return None


def sanitize_rolling_plan(
    client: ClaudeClient,
    out_path: pathlib.Path,
    client_spec: ClientSpec,
    *,
    timeout: int | None = None,
) -> None:
    plan_text = _read_rolling_plan_for_sanitize(out_path)
    if plan_text is None:
        return
    prompt = build_sanitize_prompt(plan_text, out_path)
    model = sanitize_model_for(client_spec)
    logger.info("sanitizing rolling plan (model: %s)", model)
    try:
        with_reap_after(
            client,
            timeout,
            lambda: client.run(prompt, label="handoff:sanitize", model=model),
        )
    except Exception as exc:
        _restore_rolling_plan(out_path, plan_text, f"sanitize pass failed: {exc}")
        return
    try:
        sanitized_text = out_path.read_text(encoding="utf-8")
    except OSError as exc:
        _restore_rolling_plan(
            out_path,
            plan_text,
            f"sanitize pass completed but output could not be read: {exc}",
        )
        return
    if not sanitized_text.strip():
        _restore_rolling_plan(
            out_path, plan_text, "sanitize pass completed but output was empty"
        )


def _read_optional_spec(spec_arg: str | None) -> str | None:
    if spec_arg is None:
        return None

    spec_path = pathlib.Path(spec_arg).resolve()
    if not spec_path.exists():
        sys.stderr.write(
            f"warning: --spec does not exist, continuing without north-star context: {spec_path}\n"
        )
        return None
    if not spec_path.is_file():
        sys.stderr.write(
            f"warning: --spec is not a file, continuing without north-star context: {spec_path}\n"
        )
        return None
    if spec_path.stat().st_size == 0:
        sys.stderr.write(
            f"warning: --spec is empty, continuing without north-star context: {spec_path}\n"
        )
        return None

    try:
        return spec_path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        sys.stderr.write(
            f"warning: --spec is not valid UTF-8, continuing without north-star context: {spec_path}\n"
        )
    except OSError as exc:
        sys.stderr.write(
            f"warning: failed to read --spec, continuing without north-star context: {spec_path}: {exc}\n"
        )
    return None


def _parse_client_spec(client_arg: str) -> ClientSpec:
    try:
        return ClientSpec.parse(client_arg)
    except ValueError as exc:
        die(str(exc))


def parse_args(argv: list[str]) -> argparse.Namespace:
    usage = "usage: handoff.sh --plan <path> [--spec <path>] [--out <path>] [--base <ref>] [--rev <ref>] [--client <provider:model>] [--timeout <secs>]"
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--plan", dest="plan", required=True)
    parser.add_argument(
        "--spec",
        dest="spec",
        default=None,
        help="overarching chain spec used as read-only north-star context",
    )
    parser.add_argument("--out", dest="out", default=None)
    parser.add_argument("--base", dest="base", default=None)
    parser.add_argument("--client", dest="client", default=str(PACKAGE_DEFAULT))
    parser.add_argument(
        "--timeout",
        dest="timeout",
        type=int,
        default=None,
        help="wall-clock timeout (seconds) for the main agent and the sanitize pass; on expiry the active client subprocess is reaped",
    )
    parser.add_argument(
        "--rev",
        dest="rev",
        default=None,
        help="inspect this ref instead of HEAD (e.g. a target branch name)",
    )
    return parser.parse_args(argv)


def run(client: ClaudeClient, args: argparse.Namespace) -> int:
    plan_path = pathlib.Path(args.plan).resolve()
    if not plan_path.exists():
        die(f"--plan does not exist: {plan_path}")
    if not plan_path.is_file():
        die(f"--plan is not a file: {plan_path}")
    if plan_path.stat().st_size == 0:
        die(f"--plan is empty: {plan_path}")
    try:
        plan_text = plan_path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        die(f"--plan is not valid UTF-8: {plan_path}")
    except OSError as exc:
        die(f"failed to read --plan {plan_path}: {exc}")

    spec_text = _read_optional_spec(args.spec)

    if args.out:
        out_path = pathlib.Path(args.out).resolve()
        if not out_path.parent.exists():
            die(f"--out parent directory does not exist: {out_path.parent}")
        if not out_path.parent.is_dir():
            die(f"--out parent path is not a directory: {out_path.parent}")
    else:
        out_path = auto_name_out(plan_path)

    child_plan_path = out_path.parent / (out_path.stem + "-child" + out_path.suffix)
    signal_path = out_path.parent / (out_path.stem + ".state.json")

    try:
        branch, git_log, git_diff = collect_git_context(args.base, rev=args.rev)
    except SystemExit:
        raise
    except Exception as exc:
        die(f"git context collection failed: {exc}")

    prompt = build_prompt(
        plan_text=plan_text,
        branch=branch,
        git_log=git_log,
        git_diff=git_diff,
        out_path=out_path,
        child_plan_path=child_plan_path,
        signal_path=signal_path,
        spec_text=spec_text,
    )

    client_spec = _parse_client_spec(args.client)
    logger.info("running handoff agent (client: %s)", client_spec)
    try:
        with_reap_after(
            client,
            args.timeout,
            lambda: client.run(prompt, label="handoff", model=client_spec.model),
        )
    except Exception as exc:
        sys.stderr.write(f"error: handoff agent failed: {exc}\n")
        return 1

    if not signal_path.exists():
        sys.stderr.write(f"error: signal file not written by agent: {signal_path}\n")
        return 1

    try:
        state = json.loads(signal_path.read_text(encoding="utf-8"))
    except Exception as exc:
        sys.stderr.write(f"error: could not parse signal file {signal_path}: {exc}\n")
        return 1

    exit_state = state.get("exit_state")
    if exit_state not in ("next-plan", "chain-done", "bail"):
        sys.stderr.write(
            f"error: signal file has unrecognized exit_state: {exit_state!r}\n"
        )
        return 1

    logger.info("handoff complete: %s", exit_state)
    if exit_state == "next-plan":
        child_plan = state.get("child_plan")
        if not child_plan:
            sys.stderr.write(
                "error: signal file exit_state is next-plan but child_plan is null\n"
            )
            return 1
        if not pathlib.Path(child_plan).exists():
            sys.stderr.write(
                f"error: child plan path in signal file does not exist: {child_plan}\n"
            )
            return 1
        logger.info("updated plan: %s", out_path)
        logger.info("child plan:   %s", child_plan)
        logger.info("signal file:  %s", signal_path)
    elif exit_state == "chain-done":
        logger.info("updated plan: %s", out_path)
        logger.info("signal file:  %s", signal_path)
    elif exit_state == "bail":
        reason = state.get("reason") or "(no reason given)"
        logger.info("bail reason:  %s", reason)
        logger.info("updated plan: %s", out_path)
        logger.info("signal file:  %s", signal_path)

    raw_followups = state.get("operator_followups")
    followups = (
        [str(item) for item in cast(list[Any], raw_followups) if str(item).strip()]
        if isinstance(raw_followups, list)
        else []
    )
    if followups:
        logger.info("operator follow-ups (%d):", len(followups))
        for item in followups:
            logger.info("  - %s", item)

    sanitize_rolling_plan(
        client,
        out_path,
        client_spec,
        timeout=min(args.timeout, 60) if args.timeout is not None else None,
    )
    return 0


def main(argv: list[str]) -> int:
    configure_logging()
    args = parse_args(argv)
    client_spec = _parse_client_spec(args.client)
    if client_spec.provider == "claude" and shutil.which("claude") is None:
        die("claude CLI not found")
    return run(to_client(client_spec), args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
