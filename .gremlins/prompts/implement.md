Implement the following plan.

A dedicated verify stage runs `make check && make test` after this stage
finishes. Do **not** run `make test` or `make check` yourself — the verify
stage handles both. A targeted `cargo test -p gremlins --lib -- <specific_test>`
or `cargo check` is fine for the code you changed.

<plan>
{plan}
</plan>
