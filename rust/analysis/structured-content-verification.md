# Structured message transport verification

The native backend now accepts prepared text and image URL parts through the
real `/message` HTTP endpoint. The shared Message retains its text field for
command dispatch and adds optional typed content parts for model requests.
Existing text-only JSON remains compatible. Tests stay inline in the files
they cover, following the user's requested layout.

## Runtime behavior

- Streaming and tool-capable requests preserve current content parts and
  decode structured history before sending it to the model.
- SessionDB stores arrays using the Python reference's NUL-prefixed JSON
  convention. Plain text stays plain text; malformed prefixed JSON falls back
  to its original string.
- Unsupported backends reject current structured input explicitly. The HTTP
  route rejects it before persistence or backend execution. The CLI backend
  also rejects structured history instead of silently flattening it.
- Encoding occurs before the existing database append operation. No network
  call was added inside a database critical section.

## Evidence

The inline HTTP tests bind local gateway and model servers, create a real PNG
in a temporary directory, build guarded native image parts, and POST those
parts through `/message`. They record actual model requests in both streaming
and tool modes, then submit a second turn and reopen SQLite to verify that the
first image survives replay. Another HTTP test verifies rejection by a text-only
backend without recording a user turn or invoking that backend.

`gen_content_storage_goldens.py` executes the Python storage codec and produces
14 source-derived cases. Inline storage tests compare decoded values, including
Unicode, JSON-looking plain text, structured arrays, and malformed prefixes.

Workspace validation: 1006 tests passed, one existing Python-bridge test ignored.
The full run is recorded in `takeover-content-workspace-tests.log`.

Claude implemented the storage boundary and Gemini implemented the native
request boundary. Codex reviewed those changes, added capability rejection,
generated the Python oracle, and exercised the real HTTP and SQLite paths.

## Remaining work

This accepts already prepared parts. Platform attachment downloads, pending
event consumption, session model resolution, and live STT/vision enrichment
still need runner integration. HEIC/AVIF decoding remains unsupported.

Stored JSON is equivalent after decoding, but its whitespace and Unicode
escaping are not byte-identical to Python. The Python `api_content` sidecar
and richer persisted tool-call history are not ported by this change. The
existing session lifecycle and tool-loop persistence limits still apply.
