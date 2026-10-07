---
name: roadmap
description: GitHub Projects roadmap schema and reconciliation rules for multi-agent development.
---
# Roadmap

The GitHub Project is a view over GitHub work, not a second issue tracker.

## Project schema

Create only if absent:
- Status: Backlog / Ready / In Progress / In Review / Blocked / Done
- Priority: P0 / P1 / P2 / P3
- Area: Foundation / Discord / Rust+ / Push-FCM / Database / CI-CD / Operations / Docs
- Risk: Low / Medium / High
- Target version
- Iteration

Prefer real issue/PR items over draft Project notes.

## Reconciliation rules

- open issue, unmet dependency -> Blocked;
- open issue, dependencies done, not assigned/started -> Ready;
- branch/active implementation -> In Progress;
- open ready-for-review PR -> In Review;
- merged PR closing issue -> Done;
- CI failure alone does not automatically mean Blocked if the worker is actively fixing it; use In Progress/In Review with blocker details in issue/PR;
- external credential/platform dependency -> Blocked.

Project fields summarize. Acceptance criteria, technical detail, dependency links, and agent notes stay in issues/PRs.

Workers update their own item. Roadmap agent owns schema and cross-item reconciliation.
