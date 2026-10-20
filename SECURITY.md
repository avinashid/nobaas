# Security policy

Nobaas is pre-1.0 research-grade software and has **not** had an external audit. Don't rely on it yet to protect
high-value data.

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's "Report a vulnerability" (Security advisories) on this
repository. Don't open a public issue. We aim to acknowledge within 7 days.

## Scope

- the `nobaas-core` handshake, wire format, relaying and NAT traversal;
- the C ABI (`nobaas-ffi`) and the JavaScript glue;
- the signaling adapters.

The threat model will live in `docs/THREAT-MODEL.md` (in progress).
