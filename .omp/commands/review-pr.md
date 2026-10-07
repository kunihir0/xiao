# Review PR

`$ARGUMENTS` identifies one PR number/URL.

Spawn `reviewer` and, based on changed paths/risk, `security-reviewer`, `db-engineer`, `protocol-engineer`, or `ci-engineer`. Run reviews in parallel when independent. Reviewers use GitHub MCP to leave factual findings and update Project state. Implementation fixes go back to the owning worker, not the reviewer by default.
