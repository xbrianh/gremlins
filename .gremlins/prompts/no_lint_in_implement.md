## Linting, formatting, and type checking

Do **not** run `make check`, `ruff`, `pyright`, `cargo fmt`, `cargo clippy`, or any
other linter/formatter/type-checker during implementation. These are handled
automatically by downstream pipeline stages (`normalize` + `verify`) after you
finish. Your job is correct implementation — let the tooling handle the rest.

- Do not add `# type: ignore`, `# pyright: ignore`, `#[allow(...)]`, or any
  other suppression directive to silence a checker.
- If you already ran a checker and it failed, ignore the failure and move on.