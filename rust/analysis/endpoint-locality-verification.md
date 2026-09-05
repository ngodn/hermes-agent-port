# Endpoint locality and Ollama fallback

The endpoint locality predicate and should_probe_ollama_vision gate now live in
local_probe.rs. image_routing::lookup_ollama_vision connects that gate to the
existing inference URL/key resolver and the real Ollama detection/show client.
This is the final fallback stage, not yet the full session capability lookup.

## Verification

gen_endpoint_locality_goldens.py executes the actual Python predicate using
CPython 3.12.13. The 253 cases cover private IPv4 boundaries and exceptions,
CGNAT, literal and mapped IPv6, scopes, unqualified/container hostnames,
malformed ports and addresses, userinfo, and Unicode delimiter rejection.
The generator enumerates Unicode compatibility delimiters and all decimal
digit alphabets so the fixtures also cover Python's permissive integer fallback.

Inline Rust tests compare all cases. A real HTTP test drives config URL/key
resolution through locality, detection, and /api/show, checking the bearer key
and bare model payload. A second test maps remote.example to a loopback server:
the gate sends no request, then direct detection reaches the same server. This
proves the gate did not merely mistake a network failure for remote rejection.

Workspace validation: 1,045 tests passed, one existing bridge test ignored.
Formatting and Clippy with warnings denied pass. Logs are
takeover-locality-workspace-tests.log and takeover-locality-clippy.log.

## Source behavior and corrections

Claude supplied the locality implementation and inline tests. Codex added the
source oracle and fallback integration, then corrected missing Unicode
delimiter checks, Unicode/underscore integer parsing, large-integer handling,
and scoped IPv6 parsing. Reqwest URL normalization is deliberately not used
for this predicate: it changes IPv4 shorthand and trailing-dot behavior.

The no-dot shortcut precedes IP classification in Python. Public IPv6 literals
without embedded dotted IPv4 can therefore qualify as local. This port retains
that routing behavior; it is not a general network security boundary.

The private-address and Unicode tables match the selected Python 3.12.13
reference. Other Python/Unicode versions can differ. The fixture corpus is
extensive but does not prove every possible malformed URL has identical behavior.

## Remaining integration

Config overrides, managed runtime capability, and the catalog must run before
the Ollama fallback. Session model resolution and rich adapter routing still
need their live consumers. The fallback currently accepts a bare model name;
provider-prefix resolution must use the real plugin-aware registry, not guessed
directory names. See provider-prefix-plan.md for the corrected dependency plan.
