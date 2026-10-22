---
name: review-pr
description: Review one xiao pull request at its exact head SHA by fanning out the required specialist reviewers in parallel, verifying their findings, and posting one consolidated review. Use when asked to review a PR (number or URL).
---

# Review PR

`$ARGUMENTS` is one PR number or URL in `kunihir0/xiao`.

1. Load the PR with `mcp__github__pull_request_read` (`get`, `get_files`, `get_check_runs`, `get_reviews`). Record the exact head SHA. Fetch it locally (`git fetch origin pull/<n>/head`) and check it out in a scratch worktree under the scratchpad.
2. Pick reviewers from the routing table in `CLAUDE.md`, based on the changed paths. Launch them **in one message, in parallel**. Give each a standalone brief: the PR number, head SHA, base SHA, local worktree path, the linked issue's acceptance criteria, earlier review findings to re-check, and the required output shape. Give `test-reviewer` `isolation: "worktree"` or a dedicated worktree path.
3. While they run, read the diff yourself so you can judge their findings.
4. Verify every blocking finding against the code. Drop findings you can disprove, and say why. Confirm the head SHA is unchanged.
5. Record CI state at that head (required: fmt, clippy, test, deny, shear, db; coverage is informational).
6. Run `redaction-auditor` on the draft review text, then post **one** review with `mcp__github__pull_request_review_write` (`event: COMMENT`, `commitID` = head SHA). Include a per-reviewer verdict, the blocking findings with `path:line` and fix, the CI table, and the attribution footer.
7. Report the verdict to the user. Do not merge. Merging needs the user's go-ahead, and never follows a review this session posted itself.
