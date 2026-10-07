# Rust+ adapter

This crate translates Rust+ client and push-receiver lifecycle APIs into the
application-owned pairing and session ports. Push pairing values are parsed as
bounded typed data, and protocol credentials remain redacted at the domain
boundary.

Pairing endpoints accept only publicly routable IPv4/IPv6 literals. Hostnames
are rejected to avoid DNS rebinding of an untrusted pairing payload; private,
loopback, link-local, documentation, multicast, unspecified, CGNAT, and
IPv4-mapped/compatibility forms are rejected as well.

Live pairing additionally requires Facepunch companion push-token registration
and persistence of the FCM credentials produced by registration. Those external
operations are deliberately not implemented in issue #7 and must be completed
by a follow-up before a deployment can pair servers end to end.
