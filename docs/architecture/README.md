# Architecture

Dependency direction is inward:

```text
apps/bot
  |-- bot-discord ---------\
  |-- bot-db --------------+--> bot-application --> bot-domain
  |-- rustplus-adapter ----/
  |      |-- rustplus-client
  |      `-- push-receiver
  |-- bot-config
  `-- bot-telemetry

test-support is test-only.
```

`domain` knows no Discord, Diesel, PostgreSQL, protobuf, FCM, or deployment types. `application` owns use cases and ports. Adapters translate at boundaries. The process composition root is the only place that wires concrete adapters together.

There is no web application, HTTP API, dashboard, or web health server. Operational state is exposed through process/container health, structured telemetry, Discord connection state, and database readiness until a deployment platform proves a network probe is necessary.
