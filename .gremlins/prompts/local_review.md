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

Write findings as markdown to `{artifact_dir}/{name}.md` using this structure:

**For each finding**, write a block:

```
### `path/to/file.py`, line <N>
**Category**: Correctness | Security | Performance | Readability | Testing
**Issue**: One sentence describing exactly what is wrong.
**Fix**: One sentence describing what to change.
```

- `line` is the line number in the **new version** of the file (the `+` side of the diff)
- Every finding must cite a specific file and line — no file-level or vague findings
- If there are no issues worth noting, say so explicitly with an empty findings list

End with a 2–4 sentence overall summary.
