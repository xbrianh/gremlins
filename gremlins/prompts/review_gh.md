<!-- placeholders: pr_url -->
# Review a GitHub PR and post inline comments

Review the pull request at `{pr_url}` and post the review directly to GitHub as a PR review with inline line comments.

## Step 1: Gather PR information

Fetch PR metadata:

```
gh pr view {pr_url} --json number,title,body,author,baseRefName,headRefName
```

Fetch the diff. Check the size first to pick the right strategy:

```
gh pr diff {pr_url} | wc -c
```

**If the diff is ≤ 80 000 bytes**, read it whole:

```
gh pr diff {pr_url}
```

**If the diff is > 80 000 bytes**, fetch per-file instead — the whole diff won't fit in a single Read. Get the changed-file list and diff each file individually:

```
gh pr view {pr_url} --json files -q '.files[].path'
```

Then for each path:

```
gh pr diff {pr_url} -- <path>
```

Review every file. A file whose diff is itself > 80 000 bytes is rare but possible; if it happens, read it in chunks using `offset` and `limit` on the Read tool (e.g. `limit: 500` lines at a time).

**If a `Read` call fails with "exceeds maximum allowed tokens"**, switch to per-file diffs immediately — do not retry the same Read call.

## Step 2: Review the code

Analyze every file in the diff thoroughly. For each change, evaluate:

- **Correctness**: Logic errors, off-by-ones, missing edge cases, race conditions
- **Security**: Injection, auth gaps, secrets, OWASP top 10
- **Performance**: Unnecessary allocations, N+1 queries, missing indexes
- **Readability**: Unclear naming, missing context, overly clever code
- **Testing**: Adequate coverage for new/changed behavior

Read surrounding code in the repo as needed for full context — don't review the diff in isolation.

## Step 3: Build the review

Construct a JSON body for the GitHub pull request review API. The format is:

```json
{{
  "event": "COMMENT",
  "body": "Overall summary of the review",
  "comments": [
    {{
      "path": "relative/file/path",
      "line": <line_number_in_the_new_file>,
      "side": "RIGHT",
      "body": "Comment text (markdown supported)"
    }}
  ]
}}
```

Rules for the review:
- `event` must be `"COMMENT"` (not APPROVE or REQUEST_CHANGES — leave that decision to a human)
- `line` is the line number in the **new version** of the file (the right side of the diff), corresponding to the `+` lines or unchanged context lines shown in the diff
- `side` should always be `"RIGHT"`
- Each comment `body` should be specific and actionable — say what's wrong and suggest a fix
- The top-level `body` is a concise summary (2-4 sentences) of the overall review findings
- If there are no issues worth commenting on, set `comments` to `[]` and note that in the summary
- For multi-line comments, use `start_line` and `line` to specify the range, and add `"start_side": "RIGHT"`

## Step 4: Post the review

Use `gh api` to submit the review. Get the repo owner/name from the PR metadata or by running `gh repo view --json nameWithOwner -q .nameWithOwner`.

```
gh api repos/{{owner}}/{{repo}}/pulls/{{number}}/reviews --input /dev/stdin <<< '$JSON'
```

Write the JSON to a temp file if it's large, then pass it via `--input`.

After posting, print a link to the PR so the user can see the review.

## Emit a bail marker (running under a gremlin pipeline)

After posting the review, classify your findings and — if any are blocker-severity — emit a bail marker:

The only question that matters: **can the address stage fix this without asking anyone?** If yes, do not bail — flag it in the review and move on.

- **Security blocker** (auth gaps, injection, credential exposure, OWASP top 10): run `{bail_command} security "<one-line summary>"`
- **Unfixable blocker** — the address stage cannot proceed because the spec is ambiguous, the approach is fundamentally wrong, or the required behavior is a judgment call not pinned down by the issue: run `{bail_command} reviewer_requested_changes "<one-line summary>"`
- **Everything else**: do not bail. Incomplete wiring, missing imports, dead code, wrong identifiers, off-by-ones, missing tests, simple renames — flag them and let the address stage handle them. Err strongly on the side of not bailing.

If the review has no blocker-severity findings, do not run the helper — exit normally. The bail marker is the signal the pipeline checks after this stage.

**30-second rule**: if a competent developer could fix it in under 30 seconds without asking questions — missing import, wrong identifier, off-by-one, trivial rename — do not bail; flag it in the review.
