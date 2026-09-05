# API content replay

Python's `agent/turn_context.py::substitute_api_content` restores nonempty string
sidecars on user and assistant messages. It removes every sidecar, including
invalid values and sidecars on other roles. This happens before schema projection
in the native request builder, on a fresh copy of the input messages. The direct
transport projection continues to strip internal fields without doing composition.

Evidence:

- 100 cases execute the actual Python function across roles, scalar and
  multimodal clean content, sidecar types, and missing fields.
- A real HTTP test sends an ordinary tool-capable request and then a tool-free
  summary request. Both contain the previously sent user and assistant content,
  preserve the system prefix, and omit api_content from the wire. The earlier
  message prefix is identical in both requests; source messages remain unchanged.
- Workspace: 1,172 tests passed, one existing bridge test ignored. Clippy with
  warnings denied and formatting passed.

This restores sidecars already present in native request messages. It does not
add sidecar columns to SQLite, enrich HistoryMessage, or implement generation of
per-turn gateway notes. Those persistence and composition paths still need to
reach this send boundary. Future content rewrite paths must drop stale sidecars
so replay cannot undo image eviction, redaction, or a repair merge.
