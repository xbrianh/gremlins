<!-- placeholders: text -->
A code review of the most recent implementation follows. **Default: fix every actionable finding.** Severity language ("nit", "minor", "non-blocking", "fyi") is metadata, not a routing signal — fix it anyway. The only reason to skip is if the reviewer is factually wrong (verify by re-reading the code first) or the comment is a question that needs no code change. Note any skipped findings briefly in your final summary with the reason.

---
{text}

---

After making all fixes, stage the changed files by name and create a single git commit titled 'Address review feedback' whose body references the review file. Do not push.

End with a short summary (to stdout) of: what you addressed, what you skipped and why.
