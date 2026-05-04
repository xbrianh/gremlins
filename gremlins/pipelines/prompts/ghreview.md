When writing code, follow these principles:

{code_style}

# Review a GitHub PR and post inline comments

Review the pull request at `{pr_url}` and post the review directly to GitHub as a PR review with inline line comments.

## Step 1: Gather PR information

Fetch the PR metadata and diff:

- `gh pr view {pr_url} --json number,title,body,author,baseRefName,headRefName`
- `gh pr diff {pr_url}`

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
{bail_section}
