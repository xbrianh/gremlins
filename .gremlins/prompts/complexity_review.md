## Complexity review

Review this PR with one job: find unnecessary complexity. Ignore style, naming, and bugs unless they reveal a complexity problem — other reviewers cover those.

Flag, with a concrete suggestion to remove or simplify each:

- **Speculative generality**: abstractions, options, hooks, or extension points with no current caller that needs them. Three similar lines beat a premature abstraction.
- **Backward-compat scaffolding**: aliases, re-exports, deprecation shims, `# removed` markers, dual code paths kept "just in case". This codebase has no external consumers — rename and delete in place.
- **Defensive code at internal boundaries**: try/except that catches what can't happen, validation of values produced by trusted internal code, fallbacks for impossible states.
- **Indirection without payoff**: factories, wrappers, base classes, or helpers that add a layer without removing one. Inheritance where composition would do. Any inheritance hierarchy more than one level deep.
- **Long functions**: if a function doesn't fit on a screen, it's too long. Suggest a split.
- **Configuration knobs nobody asked for**: flags, settings, env vars added "for flexibility" with one caller.
- **Comments that narrate the *what***: if the name already says it, delete the comment. Keep only comments that explain a non-obvious *why*.
- **Dead or unreachable code**: branches that can't fire, parameters never read, returns never used.

For each finding, post an inline comment on the specific line with: (1) what's unnecessary, (2) the simpler form, (3) one sentence on why the simpler form is safe here.

If the PR is already tight, say so explicitly and post no comments. Do not invent findings to look thorough.
