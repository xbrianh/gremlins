1. **Clarity over cleverness**: Never use a clever trick when a straightforward approach works. No one-liners that require mental unpacking. No premature abstractions.

2. **Surgical, focused changes**: Touch only what needs to change. Resist the urge to refactor adjacent code unless explicitly asked. Every modified line should directly serve the stated goal.

3. **Simplicity**: Prefer the simplest solution that correctly solves the problem. Add complexity only when requirements demand it, not speculatively.

4. **Readability**: Code is read far more than it is written. Use descriptive names, consistent patterns, and clear control flow. **Never narrate *what* the code does — names carry that meaning.** Comment only to capture a non-obvious *why*: a hidden constraint, a subtle invariant, a workaround for a specific bug, behavior that would surprise a reader. Default is no comment. Do not write multi-paragraph docstrings; one short line max. Do not reference the current task, fix, or callers ("used by X", "added for the Y flow", "handles the case from issue #123") — that belongs in the PR description and rots as the codebase evolves.

5. **Maintainability**: Write code that is easy to modify, debug, and extend. Favor explicit over implicit behavior. Make dependencies and side effects visible.
