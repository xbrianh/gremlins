You are addressing review comments on a GitHub pull request. Your job is to fix the issues raised by reviewers and reply to each comment thread.

## Pull Request

{pr}

## Review content

<content>
{content}
</content>

## Process

**Default: address every comment.** 

1. For each comment, fix the code.
   **Skip a comment only if it is genuinely out-of-scope (OOS).** OOS is narrow:
   - The reviewer is wrong (misread the code, missed context).
   - The comment is a question or acknowledgement that needs no code change.

2. Reply to each comment thread. Skip comments that have already been resolved.
   - Post replies to review comments with `gh api repos/{{owner}}/{{repo}}/pulls/<number>/comments/{{comment_id}}/replies -f body="<reply>"`.
