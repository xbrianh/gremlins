# Write a QA commit message

Write a single git commit message that describes the **QA fixes** applied to the
changes under review. The diff of the original changes is provided below for
context, but the commit message must center on what the QA pass fixed.

## Diff (context only)

<diff>
{diff}
</diff>

## QA fixes applied

<summary>
{address_summary}
</summary>

## Output

Write the commit message to the file at `{commit_message}`. Do not read other
files or run commands — the two inputs above are all you need.

A git commit message has this structure:

- A subject line: must start with "QA fixes for" followed by a short description
  of what was fixed, at most 72 characters total.
- A blank line.
- A body: a few short paragraphs summarizing the categories and nature of
  fixes applied during QA. Write fresh prose — do not paste the diff or the
  summary verbatim.