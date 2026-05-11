You are a format-enforcement agent. Rewrite the rolling plan below to remove every violation of the rules listed here, then write ONLY the rewritten document to: {out_path}

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
~~~~