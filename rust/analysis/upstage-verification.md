# Upstage request hook and reasoning configuration

Upstage now loads as a native provider with its complete declarative profile and
its sole overridden hook, build_api_kwargs_extras. The solar alias resolves to
that registered profile. Hook identity is attached to the profile, so alias or
name changes do not select behavior through string matching in the request path.

The native client applies profile request fields to both streaming requests and
every tool-loop request. Upstage defaults to medium, omits reasoning for disabled
or minimal settings and known non-reasoning model families, and clamps stronger
levels to high. Its direct-hook quirk is preserved: effort none without enabled
false maps to low. The normal config parser converts none to enabled false first.
Malformed truthy non-string hook efforts return an error before sending a request,
as in the main Python chat transport.

The shared native reasoning module implements nearest-weaker effort clamping,
explicit overrides, disable parsing, and config resolution with per-model spelling
variants preceding the global setting. Variant order, non-overlapping version-digit
substitution, provider/aggregator prefixes and CPython whitespace are preserved.
Native startup resolves this configuration when constructing each client. This does
not implement session slash-command changes or mutate an existing conversation.

## Evidence

The generator executes the actual Upstage class and Python reasoning module, plus
AST-extracted config resolver functions from hermes_constants.py. It emits the full
profile definition and 138 hook, 208 clamping and 215 config-resolution cases. It
also rejects newly added Upstage methods until those hooks have native equivalents.
The source oracle includes malformed values, false versus zero, control-character
whitespace, model exclusions and overlapping version separators.

Inline Rust tests compare those results. The startup HTTP test additionally makes
12 local Upstage requests through the selected solar alias, exercising defaults,
false/minimal, ultra clamping, excluded models and per-model overrides in both
streaming and tool modes. It checks the earlier conversation messages stay intact.
No live paid inference was used.

Validation: 1,084 workspace tests passed, one existing Python-bridge test ignored.
Logs: upstage-tests.log, upstage-clippy.log. Formatting and diff whitespace checks
pass, as does gen_upstage_goldens.py --check under Python 3.12.13.
Gemini's bounded source audit is in upstage-hook-source-review.md.

## Remaining scope

The shared module ports the clamping and config resolver needed by this provider;
it does not yet contain every named provider vocabulary or model-family detector
from agent/reasoning_effort.py. Request overrides, dynamic plugin discovery, other
provider hooks, runtime/session reasoning changes and the full native runner remain
open. Base profiles still have no reasoning hook; resolving a setting at startup
does not claim that their transports now project that setting. Upstage inherits
base catalog behavior; no custom catalog hook was omitted.
