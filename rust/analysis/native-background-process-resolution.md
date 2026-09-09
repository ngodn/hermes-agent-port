# Native managed-background process resolution

## Outcome

The native Unix-local terminal now starts managed, non-PTY background commands and exposes their lifecycle through a frozen `process_manage` tool. This closes the bounded local background slice without claiming PTY input, completion notifications, restart adoption, remote execution, or systemd cgroup isolation.

The tool surface is deliberately small. `terminal(background=true)` returns an opaque process ID. `process_manage` exposes only `list`, `poll`, `log`, `wait`, and `kill`. Non-PTY stdin is `/dev/null`, so `write`, `submit`, and `close` are not advertised. Unsupported PTY and notification parameters fail explicitly instead of creating a promise the native runtime cannot keep.

## Python contract evidence

[`background-process-contract-oracle.py`](../tools/background-process-contract-oracle.py) source-executes the real Python terminal, process registry, and redaction code. Its checked-in [goldens](../tools/background-process-contract-goldens.json) contain 71 cases across spawn, status, polling, log pagination, bounded waiting, exit and kill semantics, unique-prefix resolution, ownership filtering, stdin behavior, dispatch envelopes, and secret redaction. `--check` regenerates the corpus in temporary directories and compares it byte for byte.

The accompanying [contract report](native-background-python-contract.md) records the deliberately excluded Python mechanisms. AGY independently reviewed the final provider-facing behavior and found no blocking parity issue in [its parity report](native-background-parity-review-agy.md).

## Runtime boundary

One gateway-owned registry holds the process lifecycle. Conversation clients receive an owner-scoped view keyed by profile home and stable gateway session key. This makes the module deeper than a per-tool child wrapper:

- A frozen conversation client can be evicted and rebuilt without losing its running processes.
- Compression rotation preserves access because the stable route key does not change with the physical transcript ID.
- A process ID from another profile or conversation resolves as `not_found`, including prefix lookup.
- Session pruning asks the same registry whether the stable session key has active work, honoring `bg_process_max_age_hours`.
- Gateway shutdown terminates all groups in two shared bounded grace windows.

The registry owns private process groups, null stdin, concurrent output draining, incremental UTF-8 decoding, a rolling 200,000-character buffer, status publication, four-character unique-prefix resolution, wait windows, TERM-to-KILL escalation, and 30-minute retention measured from process exit. Finished output therefore remains available for the full retention window even when the command itself ran longer than 30 minutes.

Background commands reuse the conversation's isolated profile environment, persisted export snapshot, cwd, and unconditional terminal security floor. Visible command and output text is ANSI-stripped and redacted. The foreground state is read but not mutated by a background command.

## Integration proof

The live provider integration performs five requests with byte-identical frozen tool schemas. It changes cwd and exports an environment variable in foreground calls, starts a delayed background command, extracts the returned process ID from the durable tool result, waits through `process_manage`, and observes the final output. SQLite retains the stable native tool list and routed cwd throughout.

Unit coverage proves natural and nonzero exits, repeated poll tails, log pages, character-bounded split UTF-8 capture, authoritative environment construction, owner and prefix isolation, group kill, redundant kill, wait validation, redaction, schema truthfulness, and retention from exit time.

Full validation passes 1,767 Rust tests with two ignored. The selected Python process, terminal, and session-reset suites pass 197 tests with seven platform skips. The 71-case oracle regenerates byte-for-byte. Rustfmt, Ruff lint and formatting, Clippy with warnings denied, and diff hygiene pass.

## Review and helper allocation

AGY owned the Python contract oracle and later reviewed only Python parity. Claude drafted only the Rust process module and later reviewed process safety. The primary lane rejected the draft's per-client registry, incremental-poll cursor, writable non-PTY stdin, and external `kill` subprocess because those choices conflicted with the source contract or lifecycle ownership.

Claude's independent [safety review](native-background-safety-review-claude.md) found that the first integration measured finished-process retention from spawn. The primary lane corrected it to record exit time, added a regression test, converted capture to character-bounded incremental UTF-8, and bounded whole-registry shutdown. The codebase-design skill informed the gateway-owned registry boundary and the narrow owner-scoped provider interface.

## Remaining scope

The next execution slices remain native approvals and deny rules, PTY allocation plus interactive stdin, notification and watcher delivery, restart adoption, systemd cgroup isolation, remote environment backends, and delegation attribution. Until their complete contracts exist, configurations requiring them continue to use the Python path.
