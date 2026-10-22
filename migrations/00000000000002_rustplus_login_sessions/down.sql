DROP FUNCTION login_fail_orphans();
DROP FUNCTION login_state(uuid);
DROP FUNCTION login_fail(uuid, text);
DROP FUNCTION login_complete(uuid, bytea, bytea, bytea);
DROP FUNCTION login_claim(bytea);
DROP TABLE rustplus_login_audit;
-- Dropping the table drops its guard trigger.
DROP TABLE rustplus_login_sessions;
DROP FUNCTION login_sessions_guard();
