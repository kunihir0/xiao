# Test infrastructure

CI uses a disposable real PostgreSQL service. Keep test infrastructure here only when it cannot be expressed cleanly in the test harness or GitHub Actions. Do not introduce SQLite as a PostgreSQL stand-in.
