---
name: work-issue
description: Take one xiao GitHub issue from acceptance criteria to a reviewed, pushed branch by designing it, delegating bounded implementation slices, running the required specialist reviews in parallel, and verifying the integrated result. Use when asked to work on, implement, or fix an issue (number or URL).
---

# Work Issue

`$ARGUMENTS` is one issue number or URL in `kunihir0/xiao`.

1. Read the issue, its dependencies, linked ADRs and PRs (`mcp__github__issue_read`, `mcp__github__search_pull_requests`). If an open PR already covers it, continue that PR instead of competing with it. If a dependency is still open, or the issue's own Human interaction section requires a human step, stop and tell the user exactly what is blocking.
2. Create 3-7 tasks with `TaskCreate`. Decide the design yourself: crate boundaries, types, dependency choices, and whether an ADR is needed. If a third-party crate or API is involved, run `crate-researcher` first.
3. Split the work into independent slices. For each, brief a `rust-implementer`: the goal, files, acceptance criteria, tests to add, and what not to touch. Run slices whose files don't overlap in parallel, each with `isolation: "worktree"`. Do small slices yourself.
4. Integrate, then verify locally: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test` for the changed crates. Commit with a conventional message that references the issue.
5. Launch the required reviewers from the routing table in `CLAUDE.md` in parallel against that commit. Fix or explicitly answer every blocking finding, then re-review at the new head if anything material changed.
6. Run `redaction-auditor` on the diff and the planned PR text. Push the session's designated branch. Open a PR only if the user asked for one, filling in `.github/pull_request_template.md`.
7. Report the issue, branch or PR, verification results, and review verdicts. Leave merging and issue closing to the user's explicit go-ahead.
