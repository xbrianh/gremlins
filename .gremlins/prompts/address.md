Address the following issues raised by the reviewers.

A dedicated verify stage runs `make check && make test` after this stage
finishes. Do **not** run `make test` yourself. `make check` is fine to run
locally. A targeted `cargo test -p gremlins --lib -- <specific_test>` is fine
for the code you changed.

## Plan

The original implementation plan for reference:

<plan>
{plan}
</plan>

The diff under review:

<diff>
{diff}
</diff>

## Code review one

<review>
{review_one}
</review>

## Code review two

<review>
{review_two}
</review>

## Structural complexity review

<review>
{complexity_review}
</review>

## Summary

After addressing the findings, write a concise summary of every fix made to `{address_summary}`. Include the file, line, issue category, and what was changed for each fix.
