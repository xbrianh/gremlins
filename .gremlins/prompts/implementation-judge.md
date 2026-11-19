Decide from the current checkout whether the code implements the full original plan on every attempt:

<plan>
{plan}
</plan>

Inspect the implementation and any tests the plan requires. Judge what those tests exercise and assert, not whether they have been run. Do not run tests or checks, request verification evidence, or treat missing QA results as an implementation gap. Later pipeline stages handle verification, review, and fixes.

Your only writable file is `{verdict}`. Do not create, edit, or delete any other file, directly or indirectly. Leave fixes to the implementer.

If the code implements the plan, write exactly `COMPLETE` on the first line of the verdict. Write `RETRY` only for a concrete implementation gap, such as missing behavior or a required test that does not exercise the specified case. Below `RETRY`, write a standalone plan for the next implementer, who will see only this plan. Cover all remaining implementation gaps and the constraints needed to fix them; omit completed work.
