# Review local branch diff

Review the changes on the current branch against `{base_ref}` and output findings as text.

## Step 1: Gather the diff

Check diff size:

```
git diff {base_ref}...HEAD | wc -c
```

**If ≤ 80 000 bytes**, read it whole:

```
git diff {base_ref}...HEAD
```

**If > 80 000 bytes**, read per-file:

```
git diff {base_ref}...HEAD --name-only
```

Then for each file:

```
git diff {base_ref}...HEAD -- <filename>
```

A file whose diff is itself > 80 000 bytes is rare; if it happens, read it in chunks using `offset` and `limit` on the Read tool.

**If a `Read` call fails with "exceeds maximum allowed tokens"**, switch to per-file diffs — do not retry the same Read call.

## Step 2: Review the code

Analyze every file in the diff thoroughly. For each change, evaluate:

- **Correctness**: Logic errors, off-by-ones, missing edge cases, race conditions
- **Security**: Injection, auth gaps, secrets, OWASP top 10
- **Performance**: Unnecessary allocations, N+1 queries, missing indexes
- **Readability**: Unclear naming, missing context, overly clever code
- **Testing**: Adequate coverage for new/changed behavior

Read surrounding code in the repo as needed for full context — don't review the diff in isolation. Do not switch branches or fetch remote refs.

## Step 3: Output the review

Write findings as markdown to `{artifact_dir}/{name}.md`. For each issue, include the file path and line number. Keep the overall summary to 2–4 sentences. If there are no issues worth noting, say so explicitly.

## Emit a bail marker (running under a gremlin pipeline)

The only question that matters: **can the implement stage fix this without asking anyone?** If yes, do not bail — note it and move on.

- **Security blocker** (auth gaps, injection, credential exposure, OWASP top 10): end your final message with `BAIL: security: <one-line summary>`
- **Unfixable blocker** — the approach is fundamentally wrong or the required behavior is a judgment call not pinned down by the issue: end your final message with `BAIL: reviewer_requested_changes: <one-line summary>`
- **Everything else**: do not bail. Flag it and let the pipeline continue.

If the review has no blocker-severity findings, exit normally. The bail marker must be the last non-empty line of your final message.

**30-second rule**: if a competent developer could fix it in under 30 seconds — missing import, wrong identifier, off-by-one, trivial rename — do not bail.
