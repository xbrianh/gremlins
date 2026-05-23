# `gremlins/artifacts/`

Artifact registry and URI model. Standalone — nothing in `gremlins/stages/` or
`gremlins/executor/` imports from here yet.

## Public surface

```python
from gremlins.artifacts import Registry, Uri, register_scheme, EngineContext, MissingArtifact
```

## URI schemes

| Scheme | Example | Description |
|---|---|---|
| `file://session/<name>` | `file://session/handoff-1.md` | File in the run's session dir |
| `git://range/<base>..<head>` | `git://range/abc123..def456` | Commit range (SHAs) |
| `git://ref/<name>` | `git://ref/main` | Git ref → resolved SHA |
| `git://commit/<sha>` | `git://commit/abc123` | Single commit metadata |
| `gh://pr/<n>` | `gh://pr/42` | GitHub PR (url, number, branch) |
| `gh://issue/<n>` | `gh://issue/7` | GitHub issue (url, number) |

Custom schemes: call `register_scheme(scheme, resolver)` before constructing any `Registry`.

## Registry API

```python
r = Registry(session_dir=state.session_dir, cwd=state.cwd)
r.bind("plan", Uri.parse("file://session/plan.md"))
r.produced("plan")          # True
r.resolve("plan")           # Uri(scheme="file", path="session/plan.md")
r.read("plan")              # bytes from disk
r.keys()                    # iterable of bound keys
```

`read()` on an unbound key raises `MissingArtifact(key)`.

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
