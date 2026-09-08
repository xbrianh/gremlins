## Personal style preferences

These override anything that conflicts in the general code style guidance.

- **Terse**: fewer lines is better. Drop boilerplate. No ceremonial scaffolding.
- **Short functions**: if a function doesn't fit on a screen, split it. Long functions are a smell.
- **Self-documenting code**: names carry the meaning. Default to zero comments. Only comment when the *why* is non-obvious (a hidden constraint, a workaround, a subtle invariant). Never narrate *what* the code does. No multi-paragraph docstrings — one short line max.
- **No speculative generality**: no abstractions, options, or hooks for hypothetical future needs. Three similar lines beat a premature abstraction.
- **No defensive code at internal boundaries**: trust internal callers and framework guarantees. Validate only at true system boundaries (user input, external APIs).
- **Empty `__init__.py`**: package `__init__.py` should not re-export submodule symbols. Imports should reveal package structure — `from gremlins.foo.bar import Baz`, not `from gremlins.foo import Baz`.
- **No module-level globals or registration side-effects**: prefer constructor injection. Pass dependencies into `__init__`; don't mutate module state via `register_*` functions.
- **No speculative plugin hooks**: don't add extension points for hypothetical second consumers. Hand-curate; generalize only when a real second user exists.
