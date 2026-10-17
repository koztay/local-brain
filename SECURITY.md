# Security policy

local-brain runs entirely on your machine and handles source code, command output and documents
that may contain secrets. We take that seriously.

## Reporting a vulnerability

Please **do not open a public issue**. Report vulnerabilities privately via
[GitHub Security Advisories](https://github.com/koztay/local-brain/security/advisories/new).
Include steps to reproduce, the affected version (`local-brain --version`) and your OS.
We aim to acknowledge reports within 3 business days.

## Design principles

* No data leaves the machine; model requests go only to the configured local runtime.
* No network listeners: MCP uses stdio, the daemon opens no ports.
* Secret files are excluded from indexing and hook reads; secrets in outputs are masked before
  they are shown to an agent or written to the raw store.
* No telemetry.
