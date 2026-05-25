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
from gremlins.artifacts.engine import EngineContext
```

## URI schemes

| Scheme | Example | Description |
|---|---|---|
| `file://session/<name>` | `file://session/handoff-1.md` | File in the run's session dir |
| `git://range/<base>..<head>` | `git://range/abc123..def456` | Commit range (SHAs) |
| `git://ref/<name>` | `git://ref/main` | Git ref → resolved SHA |
| `git://commit/<sha>` | `git://commit/abc123` | Single commit metadata |
| `gh://pr/<n>` | `gh://pr/42` | GitHub PR (url, number, branch) via `gh pr view` |
| `gh://issue/<n>` | `gh://issue/7` | GitHub issue (url, number) via `gh issue view` |

## Registry API

```python
r = ArtifactRegistry(session_dir=state.session_dir, cwd=state.cwd)
r.bind("plan", Uri.parse("file://session/plan.md"))
r.produced("plan")          # True
r.resolve("plan")           # Uri(scheme="file", path="session/plan.md")
r.read("plan")              # bytes from disk
r.keys()                    # iterable of bound keys
```

`read()` on an unbound key raises `MissingArtifact(key)`.

In normal pipeline runs `state.artifacts` is already constructed — use it directly
rather than constructing a new registry.

## EngineContext vs artifacts

`EngineContext` carries loop identity (`loop_iteration`, `attempt`, `current_scope`)
for URI template substitution — it is **not** an artifact channel and does not read
or write files. Use `EngineContext.format(template)` to expand `{n}`, `{attempt}`,
`{scope}` in URI strings before passing them to `Uri.parse()`.

## git://range helpers

```python
from gremlins.artifacts.schemes import snapshot_head_before, bind_range_after

base = snapshot_head_before(cwd=state.cwd)
# ... run stage ...
bind_range_after(registry, "impl-commits", base, cwd=state.cwd)
```

## gh://pr capture

```python
from gremlins.artifacts._protocol import CapturingSchemeResolver

gh_resolver = registry.resolver("gh")
if isinstance(gh_resolver, CapturingSchemeResolver):
    uri = gh_resolver.capture(stdout, stderr)
    registry.bind("pr", uri)
```

## Typed read returns

`GitHubResolver.read()` returns typed objects rather than plain dicts:

- `gh://pr/<n>` → `PrInfo(url: str, number: int, branch: str)`
- `gh://issue/<n>` → `IssueInfo(url: str, number: int)`

```python
from gremlins.artifacts.schemes import PrInfo, IssueInfo

pr: PrInfo = state.artifacts.read("pr")
pr.url     # https://github.com/…/pull/42
pr.number  # 42
pr.branch  # issue-42-some-slug
```

## Registry persistence

Bindings are atomically persisted to `session_dir.parent / "registry.json"`
so they survive process restart. On construction, any existing file at that
path is pre-loaded so resumed runs see prior bindings.

## Rehydration: base_ref_sha on resume

`base_ref_sha` is bound at launch time as `git://commit/<sha>` under the key
`"base_sha"`.  `run.py` reads this from `registry.json` (not `state.json`) before
calling `Gremlin.initialize_with_runtime()` so the worktree can be created on
first start.  On resume the worktree already exists (`workdir` is set in
`state.json`), so `base_ref_sha` is not re-used by `setup_workdir`.  The binding
in `registry.json` is the authoritative source.
