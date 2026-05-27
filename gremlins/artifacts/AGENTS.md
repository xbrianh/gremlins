# `gremlins/artifacts/`

Artifact registry and URI model. The registry is wired into runs via
`state.artifacts` (an `ArtifactRegistry` instance constructed in
`gremlins/executor/gremlin.py` and stored on `State`). This wiring
applies to the main `Gremlin` executor path; subprocess child paths
(`run_child.py`, `spawn/child.py`) construct `State` without artifacts.

## Public surface

```python
from gremlins.artifacts.registry import ArtifactRegistry, MissingArtifact
from gremlins.artifacts.uri import Uri
```

## URI schemes

| Scheme | Example | Description |
|---|---|---|
| `file://session/<name>` | `file://session/handoff-1.md` | File in the run's session dir |
| `git://range/<base>..<head>` | `git://range/abc123..def456` | Commit range (SHAs) |
| `git://ref/<name>` | `git://ref/main` | Git ref name (string) |
| `git://commit/<sha>` | `git://commit/abc123` | Single commit SHA (string) |
| `gh://pr/<n>` | `gh://pr/42` | GitHub PR → `{"url", "number", "branch"}` |
| `gh://issue/<n>` | `gh://issue/7` | GitHub issue → `{"url", "number", "body"}` |

## Registry API

```python
r = ArtifactRegistry(session_dir=state.session_dir, cwd=state.cwd)

# Store a URI pointer (auto-resolved on read)
r.bind("plan", Uri.parse("file://session/plan.md"))

# Store a plain JSON value directly
r.write("status", "needs_fix")
r.write("meta", {"count": 3})

r.produced("plan")          # True
r.resolve("plan")           # Uri(scheme="file", path="session/plan.md")
r.read("plan")              # resolved value — file content as str, or dict for gh://
r.keys()                    # iterable of bound keys
```

`read()` resolves URI strings (at any nesting depth) automatically. A non-URI string or
scalar passes through untouched. Resolution is recursive: if a resolved value itself
contains URI strings, those are resolved too.

`read()` on an unbound key raises `MissingArtifact(key)`.

In normal pipeline runs `state.artifacts` is already constructed — use it directly
rather than constructing a new registry.

## Values and JSON

All values stored in the registry must be JSON-serializable. `write()` validates this
at write time via `json.dumps`. Producers and consumers own their pairwise contracts —
the registry enforces nothing beyond serializability.

URI strings stored as values (e.g. `"gh://pr/42"`) are resolved automatically on
`read()`. Consumers see the resolved content (a dict or string) rather than the raw URI.

## Read returns

Scheme resolvers return plain JSON-compatible values:

- `gh://pr/<n>` → `{"url": str, "number": int, "branch": str}`
- `gh://issue/<n>` → `{"url": str, "number": int, "body": str}`
- `git://range/<base>..<head>` → `[{"sha": str, "subject": str}, ...]`
- `git://ref/<name>` → `name` (string)
- `git://commit/<sha>` → sha (string)
- `file://session/<name>` → file content (string, UTF-8)

```python
pr = state.artifacts.read("pr")
pr["url"]     # https://github.com/…/pull/42
pr["number"]  # 42
pr["branch"]  # issue-42-some-slug
```

## git://range helpers

```python
from gremlins.artifacts.schemes import snapshot_head_before

base = snapshot_head_before(cwd=state.cwd)
# ... run stage ...
state.artifacts.bind_git_commit_range("normalize-commits", base)
```

## `{read:KEY}` URI substitution in `out:` maps

Any `out:` URI value may contain `{read:KEY}` tokens. Before the URI is parsed, each
token is replaced with the stripped content of the already-bound artifact at `KEY`:

```yaml
out:
  pr-number: file://session/pr-number.txt   # bound first
  pr: gh://pr/{read:pr-number}              # reads pr-number, expands to gh://pr/42
```

The referenced key must appear **earlier** in the `out:` map; forward references raise
`MissingArtifact`. Only `file://session/...` artifacts (resolving to a string) are
supported — passing a non-string artifact raises `TypeError`.

## Registry persistence

Bindings are atomically persisted to `session_dir.parent / "registry.json"`
so they survive process restart. On construction, any existing file at that
path is pre-loaded so resumed runs see prior bindings.

## Rehydration: base_ref_sha on resume

`base_ref_sha` is bound at launch time as `git://commit/<revspec>` under the key
`"base_sha"`.  The value is a git revspec — either a 40-char SHA (normal branch
launch) or a symbolic ref like `pull/N/head` (PR-mode launch); both are accepted
by git commands that consume it.  `run.py` reads this from `registry.json` (not
`state.json`) before calling `Gremlin.initialize_with_runtime()` so the worktree
can be created on first start.  On resume the worktree already exists (`workdir`
is set in `state.json`), so `base_ref_sha` is not re-used by `setup_workdir`.
The binding in `registry.json` is the authoritative source.
