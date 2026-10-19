# Structural complexity review

Review the architecture and module structure of the changes below. You are
reviewing **architecture**, not line-level correctness. The semantic
reviewers already handle bugs, logic errors, naming, and style — your job
is to identify structural problems that span files or modules.

## Evidence sources

Three sources are available:

1. **The plan** — what was intended.
2. **The diff** — what actually changed.
3. **The structural report** — the `cargo-modules` analysis: changed
   modules, module trees, and module-level dependency adjacency (edges
   collapsed to `module -> dep1, dep2, …`).

The report is your primary evidence. You may **read source files** to
confirm a suspected coupling leak or hotspot — the report tells you
*which* modules to look at, but reading confirms *why*. Do not read
files that are irrelevant to the changed modules.

## What you must NOT do

- Do **not** run build, test, lint, or format commands.
- Do **not** run `cargo-modules` yourself — the report already captured
  its output.
- Do **not** flag issues already covered by semantic review (bugs, typos,
  naming, style).
- Do **not** treat module cycles as problems — they are legal Rust and
  `cargo-modules --acyclic` produces false positives on item-level owns
  edges. Cross-layer coupling is the signal that matters.

## What to flag

In priority order:

1. **Cross-layer / bidirectional coupling**. A lower-layer module
   importing from a higher-layer one (e.g. a data layer importing from
   UI), or two modules importing from each other across a well-defined
   layer boundary. Use the dependency adjacency list to spot these.

2. **Fan-in / fan-out hotspots**. A module that is imported by 10+
   other modules (fan-in), or a module that imports 10+ others (fan-out).
   High fan-out often means a god module; high fan-in is fine if the
   module is a genuine utility or types crate but worth a second look.

3. **God modules**. A single module containing many public items across
   unrelated concerns. The module tree shows the hierarchy and visibility
   of every module but not item counts; confirm by reading only if the
   diff suggests unusually broad changes to a single module.

4. **Public API bloat**. Types or functions marked `pub` that are never
   used outside their own crate. The diff alone can suggest this; the
   dependency report is module-level and intra-crate only — confirm
   cross-crate callers by reading or grepping the changed sources.

## Output format

Write findings to `{filepath}` using exactly this per-finding schema
(same as the semantic reviewers, so the addressing stage can treat all
findings uniformly):

```
### `path/to/file.rs`, line <N>
**Category**: Architecture | Coupling | Layering | Fan-in/out | Public API | Module boundaries
**Issue**: One sentence describing exactly what is wrong.
**Fix**: One sentence describing what to change.
```

- `line` is the line number in the **new version** of the file (the `+`
  side of the diff)
- Every finding must cite a specific file and line — no file-level or
  vague findings
- Use the category that best describes the structural problem

If there are no structural issues worth flagging, say so explicitly:

```
### No structural findings

The changed modules fit well within the existing architecture.
No cross-layer coupling, hotspots, god modules, or API bloat detected.
```

End with a 2–4 sentence overall summary of the architecture impact of
this change.

## Reference blocks

<plan>
{plan}
</plan>

<diff>
{diff}
</diff>

<report>
{report}
</report>

**Write findings to `{filepath}`. Do not run build, test, lint, format,
or cargo-modules commands. Reading relevant source files is allowed.**
