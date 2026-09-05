# Bundled base profiles and native startup

The native registry now embeds all fields of 17 profiles from 13 bundled Python
modules: Alibaba, Alibaba Coding Plan, Arcee, Azure Foundry, Fireworks, GMI,
Hugging Face, Kilo Code, Novita, OpenAI Codex, StepFun, xAI and Xiaomi. Alibaba's
regional/plan variants account for the additional registrations.

The generator executes the actual selected declarations against the real Python
base class. It rejects top-level class/function definitions, subclass instances,
and additional instance fields. These checks catch common hook additions, but
are not a general proof that a Python module has no executable behavior.
Runtime loading uses embedded JSON and requires no Python interpreter. Headers
containing the Hermes version receive the native crate version at construction.

Native startup selects profiles by canonical name or registered alias. Explicit
native endpoint settings precede model.base_url, the profile's URL environment
setting, and its default endpoint. Registered API-key profiles use declared key
names, exclude URL variables, prefer nonempty dotenv values per name, trim keys,
and skip placeholders. Missing keys do not borrow unrelated provider keys.
Explicit native credentials retain precedence. Generic configurations retain
the earlier hostname-based resolution behavior.

The selected headers reach both native streaming and tool requests. Unsupported
API modes and missing required endpoints use the existing Python bridge. The
profile is fixed when the client is built; prior conversation messages are not
rewritten to apply a provider change.

## Validation

All tests remain inline in their owning Rust files. Registry tests compare every
loaded field and alias, then exercise a later profile replacement. A real local
HTTP server receives requests built through build_agent_client, checking alias
selection, endpoint precedence, provider headers, credentials and unchanged
history in both streaming and tool paths. The tool path reads a temporary
HERMES_HOME/.env while a stale key is present in the process environment, proving
saved-key rotation through actual startup. Environment changes are serialized
with the existing test lock and restored on exit.

Additional tests check declared key order, placeholder rejection, empty dotenv
fallback and refusal to use URL values or unrelated credentials. Fallback tests
exercise unsupported transports without invoking Python or paid inference.

Commands and logs:

- `mise exec python@3.12.13 -- python rust/tools/gen_bundled_base_profiles.py --check`
- `cargo test --manifest-path rust/Cargo.toml --workspace`, 1,082 passed, one existing bridge test ignored; `bundled-profiles-workspace-tests.log`
- `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`; `bundled-profiles-clippy.log`
- `cargo fmt --all --manifest-path rust/Cargo.toml -- --check` and `git diff --check`

## Remaining scope

These base-only bundled declarations do not replace dynamic plugin discovery or
custom provider hooks. Responses transports, external OAuth, credential pools,
profile secret-scope enforcement, and full runtime model resolution remain open.
Python’s static authentication registry also declares URL overrides absent from
some base plugin definitions; those extra mappings remain to be ported.
URL environment lookup currently reads the process environment; it does not
reproduce Python startup's dotenv-to-environment export. The existing generic
hostname-based key resolver also remains narrower than Python's runtime resolver.
No public provider inference call was made. The rich image pipeline still needs
runner construction and platform attachment/enrichment wiring.
