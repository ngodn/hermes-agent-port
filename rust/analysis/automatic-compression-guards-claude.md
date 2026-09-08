# Automatic compression guards, Claude lane

Claude owned only durable SQLite guard state in
`rust/crates/hermes-gateway/src/session_db.rs`. It did not work on trigger
policy, ingress, provider calls, or publication. The main integration lane
checked the schema and setter behavior against `hermes_state.py` before wiring
the API into automatic compression.

## Implemented state

- `compression_failure_cooldown_until REAL`
- `compression_failure_error TEXT`
- `compression_ineffective_count INTEGER NOT NULL DEFAULT 0`
- nullable `compression_recovery_deadline REAL`

Fresh databases and migration of existing databases both receive the columns.
The API loads all guard fields together, merge-maxes summary-failure cooldowns,
clears cooldown state, and atomically sets the ineffective count and recovery
deadline. Empty or missing session IDs fail closed without reporting a write.

## Main-lane correction

The first helper draft made `compression_recovery_deadline` non-null with a
zero default. Python stores the disarmed value as NULL. The final migration,
fresh schema, setter, test, and this report were corrected to preserve that
representation while the reader normalizes NULL to `0.0`.

## Runtime use

Automatic compression now:

- skips a live persisted summary-failure cooldown
- skips an armed two-strike breaker until its recovery deadline
- permits one recovery probe after expiry
- records a redacted summary failure for 600 seconds
- records non-shrinking summaries as ineffective and arms a 300-second
  recovery window on the second strike
- clears both guards after successful publication

The two-connection test proves merge-max behavior, latest-error replacement,
normalization, clearing, missing-row behavior, and persistence after reopen.
The selected Python policy/guard oracle passes 26 cases.
