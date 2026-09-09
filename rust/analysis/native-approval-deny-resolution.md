# Native approval deny-rule resolution

## Outcome

Native Unix-local terminal conversations now support a nonempty
`approvals.deny` list when `approvals.mode` is off. The terminal applies the
unconditional hardline and `sudo -S` floor first, then Python-compatible user
deny matching, before either foreground execution or background spawn.

The frozen provider schema does not change when the policy changes. Each call
reloads `config.yaml`, accepts the next valid approval policy, and retains the
last-known-good policy across read or YAML parse failures. If the mode changes
away from off, the already-frozen native terminal fails closed instead of
executing without the newly required interactive approval.

Interactive approval is deliberately not part of this checkpoint. The current
native tool call runs inside the turn that holds the session transcript lease.
An `/approve` reply would need that same lease and time out behind the blocked
tool. A safe later checkpoint needs a suspend-without-holding-the-lease control
path plus outbound request and inbound resolution wiring.

## Contract and implementation

- AGY owned the source-executed Python contract lane. Its 59-case oracle covers
  deny-list parsing, normalization, case-insensitive whole-string glob matching,
  shell-carrier payloads, compound-command boundaries, guard precedence,
  terminal envelopes, live reload, and last-known-good behavior.
- Claude owned the Rust interaction-seam audit. It established the turn-lease
  deadlock and mapped the missing tool-context and transport primitives.
- The primary lane extracted the existing glob implementation into a shared
  module, implemented the terminal policy and normalization boundary, integrated
  startup eligibility, and reviewed both helper results against production code.
- The codebase-design skill kept policy ownership behind the session-bound
  terminal tool and kept the model-visible schema immutable.

The matcher preserves Python's observed command-boundary behavior. A leading
rule such as `git push*` matches a bare command and extracted `bash -c` payload,
but does not match a later command in a compound expression or behind `sudo`.
A rule such as `*git push*` covers those wider candidates.

## Validation

- Full Rust workspace: 1,771 passed, two ignored
- Source-executed 59-case Python oracle regeneration and verification
- Selected Python approval and terminal suites: 151 passed
- Rust formatting and Clippy with warnings denied
- Python Ruff formatting and linting
- Git whitespace validation

The live provider integration runs the native terminal with a nonempty deny
list and proves byte-stable tool schemas across foreground and managed
background rounds. Focused native tests prove exact user-deny envelopes,
foreground and background blocking before process creation, hardline
precedence, live rule replacement, corrupt-YAML last-known-good retention, and
fail-closed mode changes.

## Deferred work

- Interactive manual and smart approval transport
- Lease-safe approval suspension and cancellation
- Approval prompt delivery, button callbacks, replay, and reconnect recovery
- PTY, notification, remote-backend, and delegated-process approval parity
