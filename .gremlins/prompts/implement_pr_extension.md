{spec}You are EXTENDING an existing pull request. The worktree is checked out at the PR head (detached). The PR already contains commits implementing part of the plan below; your job is to add ONLY what is still missing, as new commits on top.

The full plan from the GitHub issue:

{plan}

Before writing any code:
1. Run `git log --oneline origin/main..HEAD` to see what the PR already contains.
2. Run `git diff origin/main..HEAD` to inspect the PR's current contents.
3. Compare against the plan above. Identify the gap — the parts of the plan's acceptance criteria that the existing commits do NOT satisfy.

Then implement only the gap. Add new commits on top. You MUST commit all changes before finishing. Use `git add` + `git commit`; multiple commits are fine. Do not push (a later stage handles push). Do not amend, squash, rebase, or rewrite any existing commit on the branch — additive only. Do not modify files that the existing commits already changed unless adding new content is the only way to close the gap.

Do NOT redo work already on the branch. Do NOT create any meta/scaffolding files in the repo. The plan lives in the GitHub issue and reviews go to PR comments; the only changes in this working tree should be product code.
