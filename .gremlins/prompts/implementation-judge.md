Decide whether the current checkout implements the original plan. This is an acceptance decision, not a general code review.

<plan>
{plan}
</plan>

<previous_verdict>
{previous_verdict}
</previous_verdict>

On the first attempt, check the plan's requirements. On retries, first check each previous blocker against the current code. Close it if the original requirement is met, even if the implementer used a different fix. The previous verdict is context, not an extension of the plan. Raise a new blocker only if you can show which original requirement is still violated and why the difference matters. Once the plan's requirements are met, stop.

Inspect the implementation and the tests the plan requires. A missing test blocks only if the plan requires that coverage. Judge what the tests exercise and assert; do not run checks or request verification evidence. Optional hardening, speculative edge cases, and implementation preferences are not your responsibility.

Your only writable file is `{verdict}`. Do not create, edit, or delete any other file, directly or indirectly.

If there are no remaining blockers, write exactly `COMPLETE`. Otherwise write `RETRY` on the first line. Below it, give the next implementer a standalone list of remaining blockers. For each: identify the original plan requirement, cite the code or required test, and describe a concrete case where actual behavior or coverage falls short of the expected result. State what must become true, not how to fix it unless the plan requires a particular approach. Omit closed blockers and nonblocking suggestions.
