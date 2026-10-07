---
name: github-automation
description: GitHub-first issue, Project, branch, PR, review, Actions, and release workflow for autonomous agents.
---
# GitHub Automation

Use GitHub MCP first. The configured server intentionally exposes only `repos,issues,pull_requests,actions,projects,governance,labels` to keep tool context focused. Project writes require a token with GitHub `project` scope; when using `gh auth token`, bootstrap/auth setup should include `gh auth refresh -s project`. Use the MCP `governance` tools for rulesets and repository policy and the `labels` tools for label management. Use `gh` only for a missing operation or local git-auth bridge.

## Work item lifecycle

1. Read/create issue with problem, acceptance criteria, tests, dependencies, area, priority.
2. Add issue to the repository Project; set Ready or In Progress.
3. Branch/worktree per issue. Never direct-push main.
4. Commit conventional, focused changes.
5. Draft PR early for cross-agent visibility; link/close issue.
6. Update PR body with real test evidence and compatibility/migration notes.
7. Reviewer agent performs semantic review; CI is independent enforcement.
8. Ready PR only when findings resolved and checks green or actively running with no known failures.
9. Enable squash auto-merge when policy permits.
10. Reconcile issue + Project after merge.

## Project concurrency

Workers may mutate their own item's fields. Only `roadmap` changes Project schema/views or performs bulk reconciliation. This prevents multi-agent field races.

Every meaningful milestone sends an IRC event:

`ROADMAP_EVENT issue=<n> pr=<n|none> status=<Ready|InProgress|InReview|Blocked|Done> note=<short>`

Dependencies/blockers use:

`DEPENDENCY issue=<n> needs=<issue/pr> reason=<short>`

`BLOCKER issue=<n> reason=<short> owner=<agent-or-external>`

## Branch/PR policy

- branch examples: `feat/123-pair-server`, `fix/456-timeout-leak`, `chore/789-ci-cache`;
- squash merge to main;
- PR title is a conventional commit-style summary;
- body includes `Closes #N`, rationale, tests, migration/config/deploy impact, risk/rollback when relevant;
- no force push after review unless unavoidable; if history changes materially, tell reviewer.

## GitHub Actions policy

- least privilege `permissions`;
- explicit `timeout-minutes`;
- `concurrency` with cancellation on superseded PR runs;
- pin third-party actions to full commit SHA and annotate the human tag;
- Renovate updates action SHAs and Cargo dependencies;
- untrusted PR jobs never receive write tokens or production secrets.
