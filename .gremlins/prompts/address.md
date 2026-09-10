The plan for the implementation was:

<plan>
{plan}
</plan>

Two code reviews of the most recent implementation follow. **Default: fix every actionable finding.** Severity language ("nit", "minor", "non-blocking", "fyi") is metadata, not a routing signal — fix it anyway. The only reason to skip a finding is if the reviewer is factually wrong (verify by re-reading the code first) or the comment is a question that needs no code change. If a review comment contradicts the plan above, the reviewer is wrong — follow the plan, note the contradiction in your summary.

## Code review one

<review>
{review_one}
</review>

## Code review two

<review>
{review_two}
</review>

---

After making all fixes, stage the changed files by name and create a single git commit titled 'Address review feedback' whose body references the findings. Do not push.

End with a short summary (to stdout) of: what you addressed, what you skipped and why.
