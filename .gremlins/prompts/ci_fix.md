Review the CI check output below and decide whether corrective action is needed.

If the output indicates **all checks passed** (e.g. "ci-gate: all checks passed", "no failures", or similar), reply with a brief confirmation that CI is green and exit normally. Do **not** modify any files, run tests, or create commits.

If the output shows **failed or errored checks**, fix the code so the checks pass:

**Important constraints when fixing:**
- Fix the implementation code only.
- Do not modify CI configuration files (e.g. `.github/workflows/`).
- After fixing, stage the changed files by name and create a single git commit with a short descriptive message. Do not push.

---

**CI check output:**

{failure_output}
