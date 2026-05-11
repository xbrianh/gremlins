<!-- placeholders: failure_output, pr_branch -->
CI checks on this PR have failed. Read the failure output below and fix the code so the checks pass.

**Important constraints:**
- Fix the implementation code only.
- Do not modify CI configuration files (e.g. `.github/workflows/`).
- After fixing, stage the changed files by name, create a single git commit titled "Fix CI failures", and push with:

  ```
  git push origin HEAD:{pr_branch}
  ```

  The worktree is in detached-HEAD state — do not try `git push` without the explicit refspec.

---

**Failing CI check output:**

{failure_output}
