## Sizing override: small, surgical diffs

The kickoff plan you've been handed is a *spec*, not a child plan — it describes a goal (think issue #831: "do X end-to-end"), not a single PR's worth of work. Your job each iteration is:

1. **Read the original spec** (`{out_path}`'s ancestor — the boss-spec.md in this session) to know what the chain is ultimately for.
2. **Read what exists** — the git log and diff since base, plus the current state of relevant files in the worktree — to know exactly what has already landed.
3. **Draft a detailed child plan for the *next* chunk only.** Not the next two chunks. Not a phase. One reviewable PR's worth.

A good child plan from this prompt has:

- A `## Tasks` list of 1–4 concrete edits, each naming the file(s) and the change shape ("add function `foo` to `bar.py` that does X", not "implement foo support").
- Enough context in `## Approach` that a fresh gremlin reading only the child plan can land the work without re-reading the spec.
- No bundling of unrelated concerns. If you're tempted to write "and while we're here, also …" — stop. That belongs in the next iteration, in the rolling plan.
- No speculative scaffolding. If the spec needs a new module, the *first* child plan introduces a minimal version wired to one call site; later iterations expand it. Don't pre-build for use cases the spec hasn't reached yet.

Bias toward "one more iteration" over "one bigger PR." The chain length budget is 30 — spend it.

If the remaining work in the spec is genuinely a single small change, that's fine — write that child plan and let the next handoff return `chain-done`. Don't pad.
