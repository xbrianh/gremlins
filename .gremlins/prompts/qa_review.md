# Review a commit range

Review the changes in the diff provided below.

These commits make up the change under QA. Review the diff below and only the diff below. Do not read any source files, do not look at anything outside the diff. If you cannot reach a conclusion from the diff alone, say so explicitly and explain why.

**Do not run any tests, checks, linters, or build commands.** This is a code review only.

The diff:

<diff>
{diff}
</diff>

## Output the review

Write findings as markdown to `{filepath}` using this structure:

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

**Reminder: review the diff above. Do not read source files, do not run tools, do not look at anything outside the <diff> block.**