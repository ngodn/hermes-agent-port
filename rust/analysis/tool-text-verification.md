# Tool-template marker cleanup

The native loop removes bare ASCII bracketed markers from assistant content
alongside a validated tool batch, following `_STALE_MARKER_RE` in the Python
conversation loop. It runs after batch filtering and before the assistant replay
is appended. Ordinary text, non-ASCII markers, and plain final answers remain
unchanged. All-invalid name-retry batches bypass this cleanup.

17 fixtures execute the actual Python regex and run through the native tool
loop. They verify the next model step sees the expected assistant content,
including Python whitespace behavior and exact marker boundaries. Workspace:
1,199 tests passed, one existing bridge test ignored. Clippy and formatting pass.

This removes protocol scaffolding from replay. The wider post-tool fallback
policy still needs to track visible tool-associated answers, housekeeping-only
batches, substantive-tool invalidation, and empty final responses. It is not yet
complete post-tool answer recovery.
