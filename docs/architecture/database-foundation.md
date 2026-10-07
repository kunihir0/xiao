# Database foundation

`bot-db` is the only crate that owns Diesel, PostgreSQL, and deadpool types.
The application startup sequence creates the pool, applies embedded pending
migrations, and pings PostgreSQL before readiness is reported.

## Baseline migration safety

`00000000000000_baseline` contains only `SELECT 1` in both directions. It
establishes the migration history boundary without inventing domain tables.
The forward path is safe on a fresh database and is idempotent because Diesel
records the migration version in `__diesel_schema_migrations`. The reverse path
is also safe: it removes no application data or schema objects. Future
production schema changes must use expand/migrate/contract when destructive
changes are required; this baseline does not provide rollback for later data
changes.
