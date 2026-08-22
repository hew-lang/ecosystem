# Security

## Reporting a vulnerability

Report privately through GitHub's advisory form:
<https://github.com/hew-lang/ecosystem/security/advisories/new>. Do not open a
public issue for a vulnerability.

Include the affected package and version, what an attacker can do, and the
smallest reproduction you have. You will get an acknowledgement, and a fix or
an explanation of why the behaviour is intended.

## Scope

These packages wrap credentials and network services — database clients, S3,
OAuth, and message brokers — so connection strings, tokens, and signing
material pass through them. In scope: anything that leaks a credential,
mishandles TLS or certificate verification, mishandles a handle after close,
or lets untrusted input reach a query, a path, or a template unescaped.

Also in scope is memory safety in the native crates. Each package's C ABI layer
uses `unsafe` to bridge Rust and Hew, and a use-after-free or out-of-bounds
read reachable from safe Hew code is a vulnerability, not a bug.

Out of scope: vulnerabilities in the upstream services themselves, and in the
Hew compiler — report those to
[hew-lang/hew](https://github.com/hew-lang/hew/security).
