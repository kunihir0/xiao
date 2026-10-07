# Operations

Runbooks are added with the capability they operate. Before production launch, cover at minimum: deploy/rollback, failed migration, database restore test, Discord outage/reconnect, bot token rotation, rate-limit incident, and recovery from a bad release.

A backup policy is incomplete until restore is exercised. A release is incomplete if its database change prevents the documented application rollback path.
