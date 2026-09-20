# Write a commit message

Describe the changes below as a git commit message.

## The plan

<plan>
{plan}
</plan>

## The diff

<diff>
{diff}
</diff>

## Fixes made in response to review

<summary>
{address_summary}
</summary>

## Output

Write the commit message to the file at `{commit_message}`. Do not read other
files or run commands — the three inputs above are all you need.

A git commit message has this structure:

- A subject line: a concise, imperative-mood summary of the change, at most
  72 characters.
- A blank line.
- A body: a few short paragraphs describing what changed and why, including
  any notable fixes. Write fresh prose — do not paste the plan or diff
  verbatim.