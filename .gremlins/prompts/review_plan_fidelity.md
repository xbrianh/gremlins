<!-- placeholders: pr_url -->
# Review whether the PR actually implements the plan

The pull request at `{pr_url}` was opened to implement a specific plan (the linked issue body). Your job is to verify that the diff *actually does what the plan said*, then post a PR review with line comments for any gaps.

This review is **not** about code quality, style, security, or test coverage — those are covered by the parallel review. Focus solely on **plan fidelity**.

## Step 1: Gather context

- `gh pr view {pr_url} --json number,title,body,author,baseRefName,headRefName,closingIssuesReferences`
- `gh pr diff {pr_url}`
- Read the linked issue body in full — it is the source of truth for what should have been implemented. If the PR has multiple linked issues, read each.

## Step 2: Compare diff against plan

Walk through the plan section by section. For each concrete claim or required change, check whether the diff actually contains it. Categorize each claim:

- **Implemented** — the diff does what the plan said.
- **Partially implemented** — the diff makes a related change but skips, weakens, or differs from what the plan asked for.
- **Missing** — the plan called for it; the diff does not contain it.
- **Unrequested** — the diff makes a change that the plan did not call for. Note these but do not flag unless they conflict with the plan.

Read surrounding repo code as needed to confirm what the diff actually does — do not trust the diff in isolation.

## Step 3: Build the review

Construct a JSON body for the GitHub PR review API:

```json
{{
  "event": "COMMENT",
  "body": "Plan-fidelity summary: <N> of <M> plan items implemented; <K> partial; <J> missing.",
  "comments": [
    {{
      "path": "relative/file/path",
      "line": <line_number_in_new_file>,
      "side": "RIGHT",
      "body": "Plan called for X here, but the diff does Y. Specifically: ..."
    }}
  ]
}}
```

Rules:
- `event` must be `"COMMENT"`.
- Comments anchor to the line where the gap is most visible. For an entirely missing change, anchor near the most relevant edited line and explain that the plan required additional work elsewhere.
- The top-level `body` is a 2–4 sentence plan-fidelity summary listing missing/partial items by name.
- If everything in the plan is implemented, set `comments` to `[]` and say so in the summary.
- Do not duplicate findings the regular code review would catch (style, naming, missing tests). Stay on plan fidelity.

## Step 4: Post the review

```
gh api repos/{{owner}}/{{repo}}/pulls/{{number}}/reviews --input /dev/stdin <<< '$JSON'
```

Get owner/name from PR metadata or `gh repo view --json nameWithOwner -q .nameWithOwner`. Print the PR URL after posting.

## Emit a bail marker (running under a gremlin pipeline)

After posting, decide whether to bail:

- **Unfixable blocker** — the plan is ambiguous, contradictory, or fundamentally misaligned with what the diff is doing such that the address stage cannot reconcile them: `{bail_command} reviewer_requested_changes "<one-line summary>"`
- **Otherwise**: do not bail. Missing or partial items the address stage can implement → flag them and exit normally.

If there are no blocker-severity findings, exit normally without invoking the bail helper.
