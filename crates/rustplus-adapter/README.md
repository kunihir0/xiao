# Rust+ adapter

This crate translates Rust+ client and push-receiver lifecycle APIs into the
application-owned pairing and session ports. Push pairing values are parsed as
bounded typed data, and protocol credentials remain redacted at the domain
boundary.

Live pairing additionally requires Facepunch companion push-token registration
and persistence of the FCM credentials produced by registration. Those external
operations are deliberately not implemented in issue #7 and must be completed
by a follow-up before a deployment can pair servers end to end.
