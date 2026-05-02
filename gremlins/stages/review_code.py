"""Single-lens code review stage.

One reviewer spawns a ``claude -p`` via the injected client. ``set_stage``
updates the gremlin's sub_stage before and after the reviewer runs.
"""

from __future__ import annotations

import pathlib

from ..clients.claude import ClaudeClient
from ..state import emit_bail, set_stage

LENSES_DIR = pathlib.Path(__file__).resolve().parent.parent / "prompts" / "lenses"


def load_detail_lens() -> str:
    """Return detail lens prose. Raises if missing or empty."""
    path = LENSES_DIR / "detail.md"
    if not path.exists() or path.stat().st_size == 0:
        raise FileNotFoundError(f"missing or empty lens file: {path}")
    # Explicit utf-8 — lens files contain em-dashes and other non-ASCII, so
    # relying on the process default encoding would crash under a non-UTF-8
    # locale (e.g. a minimal container with LANG=C).
    return path.read_text(encoding="utf-8")


def run_review(
    *,
    client: ClaudeClient,
    model: str,
    out_file: pathlib.Path,
    focus: str,
    context: str,
    code_style: str,
    where_field: str,
    label: str,
    raw_path: pathlib.Path,
) -> None:
    """Invoke the reviewer. CONTEXT describes what is being reviewed;
    FOCUS is the lens prose; WHERE_FIELD is the field label used to cite
    findings (e.g. `**File:** path:line` for code reviews)."""
    style_preamble = ""
    if code_style:
        style_preamble = (
            "Follow these coding-style rules and flag violations — long functions, "
            "inheritance where functions suffice, dead comments, speculative "
            "abstractions — alongside the correctness, security, and performance "
            "findings in the focus section below:\n\n"
            f"{code_style}\n\n"
        )
    prompt = f"""{style_preamble}Read surrounding code as needed — don't review in isolation.

{context}

Structure your review as markdown:

# Review ({model})

## Summary
2-4 sentences overall.

## Findings
For each actionable finding:
### <short title>
- {where_field}
- **Severity:** blocker | major | minor | nit
- **What:** what's wrong
- **Fix:** concrete suggestion

If there are no issues worth raising, write a Findings section that says so explicitly.

Do NOT make any code changes — only write the review file.

{focus}

`{out_file}` is the canonical and required location for your review output in every case, including any short-circuit one-liner the lens tells you to emit. Do not emit the verdict only to chat; write it to `{out_file}` and then stop."""
    client.run(prompt, label=label, model=model, raw_path=raw_path)


def run_review_code_stage(
    *,
    client: ClaudeClient,
    session_dir: pathlib.Path,
    plan_text: str,
    detail: str,
    is_git: bool,
    code_style: str,
) -> pathlib.Path:
    """Execute the review-code stage: load the detail lens, run one reviewer,
    and return the output path. Emits bail_class=other on failure when
    running under a gremlin (no-op otherwise). Shared by the orchestrator
    and /localreview.

    Passing ``plan_text=""`` (empty string, not None) intentionally omits
    the plan block from the review prompt entirely — this is the contract
    that lets standalone ``/localreview`` callers run without ``--plan``.

    Stale ``review-code-detail-*.md`` files are unlinked before spawning
    the reviewer so a ``--resume-from review-code`` with a different ``-b``
    model cannot leave two files for the same lens (which would later
    confuse ``run_address_code_stage``'s glob-based discovery).
    """
    review_code = session_dir / f"review-code-detail-{detail}.md"

    for stale in session_dir.glob("review-code-detail-*.md"):
        try:
            stale.unlink()
        except OSError:
            pass

    focus = load_detail_lens()

    if is_git:
        code_scope = (
            "Review the changes introduced by the most recent commit "
            "(HEAD vs HEAD~1) plus any uncommitted working-tree changes. "
            "Use `git diff HEAD~1 HEAD` and `git diff` to see the scope."
        )
    else:
        code_scope = (
            "Review the uncommitted changes in this directory (`git diff` if "
            "available, otherwise inspect recently modified files)."
        )
    # Omit the plan block entirely when no plan was supplied (standalone
    # /localreview without --plan); sending a bare "The plan for this change
    # is:" header with empty body would confuse the reviewer.
    if plan_text:
        code_review_context = (
            f"The plan for this change is:\n\n{plan_text}\n\n{code_scope}"
        )
    else:
        code_review_context = code_scope

    # Wrap so any infrastructure failure (claude -p crash, missing output
    # file, etc.) records bail_class=other before the exception propagates.
    try:
        set_stage("review-code", {"detail": f"running ({detail})"})
        run_review(
            client=client,
            model=detail,
            out_file=review_code,
            focus=focus,
            context=code_review_context,
            code_style=code_style,
            where_field="**File:** `path/to/file.ext:<line>`",
            label=f"review-code:detail:{detail}",
            raw_path=session_dir / f"stream-review-code-detail-{detail}.jsonl",
        )
        set_stage("review-code", {"detail": f"done ({detail})"})
        if not review_code.exists() or review_code.stat().st_size == 0:
            raise RuntimeError(f"review {detail} did not produce {review_code}")
    except (SystemExit, Exception) as exc:
        emit_bail("other", f"review-code stage failed: {exc}"[:200])
        raise

    return review_code
