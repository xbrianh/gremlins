## Personal style preferences

These override anything that conflicts in the general code style guidance.

- **Terse**: fewer lines is better. Drop boilerplate. No ceremonial scaffolding.
- **Short functions**: if a function doesn't fit on a screen, split it. Long functions are a smell.
- **Self-documenting code**: names carry the meaning. Default to zero comments. Only comment when the *why* is non-obvious (a hidden constraint, a workaround, a subtle invariant). Never narrate *what* the code does. No multi-paragraph docstrings — one short line max.
- **Backward compatibility is not a concern. At all.** There are no external consumers, no published API, no downstream users to protect. Do not preserve old APIs, old call sites, deprecated paths, or compat shims. Do not add aliases, re-exports, "transition" layers, or `# removed` comments. Rename, delete, reorder arguments, and change signatures freely — update every call site in the same change. **Planners and implementers: do not spend a single sentence considering backward compatibility — pick the best design as if the old code didn't exist. Reviewers: do not flag missing backward-compat handling; if you see a reviewer raising it, that is a wrong review.**
- **No speculative generality**: no abstractions, options, or hooks for hypothetical future needs. Three similar lines beat a premature abstraction.
- **No defensive code at internal boundaries**: trust internal callers and framework guarantees. Validate only at true system boundaries (user input, external APIs).
- **Reuse `gremlins/utils/`**: `proc.py`, `git.py`, `github.py` already wrap subprocess, `git`, and `gh`. Use them instead of calling `subprocess.run` or shelling `git`/`gh` directly.
- **Empty `__init__.py`**: package `__init__.py` should not re-export submodule symbols. Imports should reveal package structure — `from gremlins.foo.bar import Baz`, not `from gremlins.foo import Baz`.
- **No module-level globals or registration side-effects**: prefer constructor injection. Pass dependencies into `__init__`; don't mutate module state via `register_*` functions.
- **No speculative plugin hooks**: don't add extension points for hypothetical second consumers. Hand-curate; generalize only when a real second user exists.
