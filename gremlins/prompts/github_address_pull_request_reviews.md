<!-- placeholder: {pr} (the PR URL, resolved from the `pr` artifact via `in: {{pr: pr}}`) -->
You are addressing review comments on a GitHub pull request. Your job is to fix the issues raised by reviewers and reply to each comment thread.

## Pull Request

{pr}

Fetch PR metadata: `gh pr view {pr} --json number,title,body,author,baseRefName,headRefName`. Store the PR number for use in API calls below.

## Review comments

Fetch review comments by running: `gh api repos/{{owner}}/{{repo}}/pulls/<number>/comments --paginate` (strip any `#` prefix from the PR number).

## Issue comments

Fetch issue comments using `gh pr view <number-or-ref> --comments`.

## Process

**Default: address every comment.** Severity language ("nit", "minor", "non-blocking", "fyi", "tracking issue acknowledged") is metadata, not a routing signal. Reviewers use those words to communicate priority to a human; you should still fix the thing. The work to fix a typo or rename a variable is trivial; refusing to do it because the reviewer was polite about flagging it is the wrong outcome.

1. Read all review comments carefully.
2. For each comment, fix the code. Walk through:
   a. Understand what the reviewer is asking for.
   b. Read the relevant code for context.
   c. Make the fix.

   **Skip a comment only if it is genuinely out-of-scope (OOS).** OOS is narrow:
   - The fix requires substantial work tracked in a separately-filed issue (the comment cites an issue number, or the work is large enough that a separate PR is the right unit).
   - The reviewer is wrong (misread the code, missed context). Verify before deciding this — re-read the code first.
   - The comment is a question or acknowledgement that needs no code change.

   "Reviewer called it a nit" is **not** a reason to skip. "Reviewer said it's tracked elsewhere" is only a reason to skip if there's a real issue cited and the work is genuinely substantial.

3. After making all changes, stage, commit, and push:
   a. `git add` the changed files (by name, not `-A`).
   b. `git commit` with a message summarizing what was addressed.
   c. `git push` to the PR branch.

   If you genuinely had nothing to fix (every comment was a question, the reviewer was wrong everywhere, or the only flagged items were tracked-elsewhere with cited issue numbers), skip 3a–3c and proceed directly to step 4 — `git commit` would fail with "nothing to commit". This case should be rare; defaulting to it is a smell.

4. Reply to each comment thread (after the push succeeds, if step 3 ran). Skip comments that have already been resolved.
   - For comments you addressed: reply briefly acknowledging the fix.
   - For questions or acknowledgements: reply briefly.
   - For OOS comments: run the OOS triage in the next section before replying.
   - Post replies to review comments with `gh api repos/{{owner}}/{{repo}}/pulls/<number>/comments/{{comment_id}}/replies -f body="<reply>"`.
5. Summarize what was done, including any issues filed for OOS comments and any `gh issue create` failures.

## Out-of-scope triage (file issues for real defects)

For the rare comment that's genuinely OOS, decide whether it looks like a real defect or noise. Real defects survive as filed issues; pure noise doesn't pollute the issue tracker.

**File a new issue when the OOS comment flags any of:**

- A bug or regression
- A security or data-correctness concern
- A performance pathology
- A hidden invariant being violated
- Anything else that, if true, would warrant a fix in some future PR

**Don't file an issue when:**

- The reviewer's claim is wrong — they misread the code, missed context, or are factually incorrect.
- The reviewer cited an existing issue (use that one; don't dupe).
- The comment is already addressed elsewhere (e.g., another comment in the same review covers the same point).

**Tie-breaker:** if you're genuinely unsure whether the comment is a real defect or the reviewer is wrong, **file the issue**. Over-filing is cheap; losing a real bug is not. A human triaging the issue can close it as invalid.

### Filing the issue

Use `gh issue create` with:

- **Repo**: always pass `--repo <owner>/<repo>` explicitly, derived from the same `gh pr view` data already fetched (e.g., `gh pr view <number> --json baseRepository`). This avoids silently filing in a fork's tracker.
- **Title**: a short distillation of the comment (no special prefix). Aim for a sentence fragment that names the defect, not the comment ID.
- **Body**: must stand on its own — a reader should not need to chase the PR to understand the issue. Include:
  - A short summary of the defect in your own words.
  - The reviewer's comment, quoted or summarized. **Before quoting, scan for secrets or sensitive data (credentials, API keys, tokens, internal URLs, customer data). If present, redact them with `[redacted]` — or if redaction isn't clean, skip filing and end your final message with `BAIL: secrets: <one-line reason>` instead. Issues are public and permanent.**
  - A PR cross-link (`Ref #<pr-number>` for same-repo, `Ref <owner>/<repo>#<pr-number>` cross-repo).
  - A permalink to the originating review comment (`html_url` from the API response).

Example invocation:

```
gh issue create --repo <owner>/<repo> --title "<short distillation>" --body "$(cat <<'EOF'
<summary>

Reviewer comment:
> <quoted or summarized comment, with any secrets redacted>

Ref #<pr-number>
<permalink to review comment>
EOF
)"
```

Capture the issue number/URL from the command's output for the reply.

### Replying on the PR thread

- **Issue filed**: reply `Filed as #N` (or `Filed as <issue-url>`) so the reviewer and any later reader can find it.
- **No issue (noise / reviewer wrong / already addressed)**: reply with a brief dismissal reason — enough that a human skimming the PR understands why no further action was taken.

### When `gh issue create` fails

Issue-creation failure is not a reason to stop addressing the rest of the PR. Do **not** write a bail marker.

- Log the failure prominently in the run output (a clear `ERROR: failed to file issue for comment <id>: <error>` line, and include it in the final summary).
- Fall back to a reply on the PR thread that **clearly marks itself as a failed filing attempt**. Open with `Tried to file as "<intended title>" but \`gh issue create\` failed: <error>. Please file manually if this is a real defect.` followed by the one-line summary. Do not phrase it as a generic dismissal.
- Continue with the remaining comments.
