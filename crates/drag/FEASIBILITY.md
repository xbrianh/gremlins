# Production source ownership: feasibility gate

## Result

Blocked with the installed `cargo-modules 0.28.0`. No production behavior was
changed. The plan requires stopping if the tooling cannot supply source
ownership; implementing a separate module resolver is explicitly out of scope.

## Checks

Ran `structure` and `dependencies` against the `gremlins` library with
`-p gremlins --lib --no-default-features`, both with and without `--cfg-test`.
Target and features were unchanged between runs. Used
`--no-fns --no-traits --no-types`, retaining ownership edges for the ownership
check. Also checked `structure --verbose` and the `orphans` interface.

The output supplies logical module names, hierarchy, and `owns` edges. It does
not supply the mapping from those modules to backing source files, distinguish
inline bodies from external declarations, or provide their source ranges.
Verbose structure output adds package/target context, not source ownership.

The requested `executor::stage_invocation_tests` witness is absent from this
checkout's source and its test-enabled module tree. Its historical 11,291-token
score therefore could not be verified here.

## Small-crate probe

A temporary standalone crate used this root:

```rust
#[path = "tests.rs"]
pub mod isolated;
#[cfg(test)]
#[path = "support/checks.rs"]
mod checks;
```

`tests.rs` contained an isolated production function. `support/checks.rs`
contained a test and an ungated `mod child;` declaration, backed by
`support/checks/child.rs`.

With the library target and features fixed:

- Production `structure` and the ownership-enabled graph include `isolated`,
  despite it having no dependency edges. A production selection must retain it.
- Adding `--no-owns` drops `isolated` from the dependency graph altogether.
  Absence from that graph cannot establish test-only ownership.
- Enabling `--cfg-test` adds `checks` and `checks::child`, with the expected
  logical ownership edge between them.
- Neither graph nor tree identifies their backing files. In particular, the
  production module's name does not identify its `tests.rs` source file.
- `orphans` reported the production `tests.rs` as orphaned, both with and
  without `--cfg-test`; it is not a reliable replacement for source ownership.

These checks establish the specific missing information: a reliable
module-to-source-file ownership mapping, including external test descendants.
Logical module names alone cannot select survey-visible files while preserving
production files such as this `tests.rs` without introducing the prohibited
resolver logic.

Temporary probes were removed. Relocation/cache acceptance tests and production
changes were not attempted because the prerequisite feasibility gate failed.
