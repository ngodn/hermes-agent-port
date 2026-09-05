# Provider registration and live vision lookup

`provider_registry.rs` implements registration, exact alias lookup, identity-based
listing, and provider-prefix recognition. `ProviderProfile` carries the declarative
base fields, including distinct inherited/omitted/fixed temperature states. It
has not yet acquired provider-specific client hooks or automatic discovery.

## Registration evidence

`gen_provider_registry_goldens.py --check` executes the reference register/get/list
functions and prefix helper. Six registration transitions verify replacement
position, aliases outliving replacement, alias priority over canonical names,
reassignment, shared identity after a profile rename, and list-cache isolation.
The 265 prefix cases cover registered and unknown names, whitespace, HTTP guards,
Ollama tags, Unicode digits, and case-insensitive matching.

The comparison caught Python's extra dotted/dotless-I matches in re.IGNORECASE;
Rust now preserves such Ollama tags. Registry names themselves are not normalized.
Prefix recognition normalizes only the prefix it passes to registry lookup.

## Live lookup evidence

`image_routing::LiveVisionLookup` now implements the existing lookup trait:

1. Borrow the named runtime provider only when provider and model match.
2. Honor an explicit capability override, including false.
3. Return unknown for absent provider or model.
4. Consult managed runtime state and staged-model capabilities.
5. Consult the cloud catalog with cold network access allowed.
6. Probe eligible Ollama endpoints, stripping only recognized provider prefixes.

Inline HTTP tests exercise the whole sequence with shared real caches and temporary
files. They verify zero requests after an override, managed false preventing cloud
lookup, cloud false preventing local probes, and a cloud miss reaching
endpoint/key resolution and prefix-aware Ollama lookup. They also call the real image
mode decision through this lookup. Separate identity tests prevent borrowing a
named custom provider from another model or provider and preserve explicit names.
Existing endpoint/auth tests continue to verify bearer forwarding.

Workspace: 1,072 passed, one existing bridge test ignored. Formatting, Clippy with
warnings denied, and diff whitespace checks pass. Logs:
`provider-registry-workspace-tests.log`, `provider-registry-clippy.log`,
`live-vision-tests.log`. Gemini source audit: `provider-registration-source-review.md`.

## Remaining integration

This is a native registration API and a live lookup that consumes it. It does not
pretend plugin filenames establish provider identity. Discovery still needs actual
entry-point invocation, bundled/user/flat-installed plugin execution, enable/disable
policy, import failure boundaries, and legacy modules. Native provider hooks and
runner construction remain unfinished. The registry deliberately accepts mutable
shared profiles to preserve reference identity semantics.

Tag matching folds digits using the shared CPython 3.12 Unicode 15 table and
handles dotted/dotless I explicitly. Cases include newer digit blocks that the
reference must leave unrecognized. OS-specific plugin/runtime behavior and the complete rich inbound runner
are not proven by these lookup tests.
