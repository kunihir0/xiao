# Rust+ adapter

This crate translates Rust+ client and push-receiver lifecycle APIs into the
application-owned pairing and session ports. Push pairing values are parsed as
bounded typed data, and protocol credentials remain redacted at the domain
boundary.

Pairing endpoints accept only publicly routable IPv4/IPv6 literals. Hostnames
are rejected to avoid DNS rebinding of an untrusted pairing payload; private,
loopback, link-local, documentation, benchmarking, reserved (`0/8`, `240/4`,
`192.0.0.0/24`), multicast, unspecified, and CGNAT ranges
are rejected, as is every IPv6 form that embeds or translates IPv4
(IPv4-mapped/compatible, NAT64 `64:ff9b::/96`, 6to4, Teredo).

Live pairing additionally requires Facepunch companion push-token registration
and persistence of the FCM credentials produced by registration. Those external
operations are deliberately not implemented in issue #7 and must be completed
by a follow-up before a deployment can pair servers end to end.
