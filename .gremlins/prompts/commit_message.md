# Write the commit message

Summarize the work done by this gremlin into a single commit-message file
used when landing the changes.

## Inputs

The original implementation plan:

<plan>
{plan}
</plan>

The diff of changes made so far:

<diff>
{diff}
</diff>

A summary of the review findings that were addressed:

<summary>
{address_summary}
</summary>

## Output

Write the commit message to the file at `{commit_message}`. Do not read source
files or run tools — the inputs above are all you need.

Format the file exactly like a git commit message:

- First line: a short title, imperative mood (e.g. "Add commit-message stage"),
  at most 72 characters.
- A blank line.
- A detailed description: a few short paragraphs describing what changed and
  why, including any notable review fixes. Plain prose — do not paste the plan
  or diff verbatim.