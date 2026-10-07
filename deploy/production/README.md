# Production deployment

Production deployment is intentionally unspecified until the target platform is chosen. Requirements when added: immutable artifact from a known commit, non-root runtime container where applicable, secret-store injection, graceful SIGTERM/SIGINT, migration compatibility checks, rollback/runbook coverage, and no embedded development infrastructure.
