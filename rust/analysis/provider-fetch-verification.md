# Base provider model-list hook

`ProviderProfile::fetch_models` now performs the base profile's native HTTP
model-list request. `get_hostname` and endpoint selection are implemented in the
same owning module, with tests inline.

A caller-supplied inference URL overrides `models_url` only when it differs from
the profile default after removing trailing slashes. An unchanged caller default
preserves the separate catalog endpoint. Headers are applied in source order:
Bearer key, Accept, supplied Hermes User-Agent, then provider overrides.

## Evidence

`gen_provider_fetch_goldens.py --check` executes the real Python base class with
its credentialed urllib boundary controlled. The 49 fetch cases compare endpoint
selection and result parsing, including duplicates and non-string IDs. Twelve
hostname cases compare explicit overrides and parsed hostnames.

Three real HTTP tests check:

- Custom endpoint precedence and headers on the actual wire.
- Same-origin forwarding, a cross-origin redirect, and a return to the original
  origin. Only Accept and User-Agent survive the cross-origin hop; credentials
  are never restored from the original header set.
- Redirect-cycle limits, HTTP failure, and invalid JSON returning no model list.

These tests complement the source cases rather than pretending their controlled
opener exercises TLS or the real wire. The workspace passes 1,076 tests with one
existing bridge test ignored. Formatting, Clippy with warnings denied, and diff
whitespace checks pass. Logs: `provider-fetch-workspace-tests.log`,
`provider-fetch-clippy.log`, `provider-fetch-tests.log`.
Gemini's source audit is `provider-model-fetch-source-review.md`.

## Remaining work

Provider discovery still needs executable registration and hook dispatch. This
turn added a real shared hook those providers can use; it did not replace imports
with manifest-name registration. Provider-specific model-list overrides and other
client hooks remain unported.

CA bundle environment precedence, multi-certificate loading, custom trust stores,
and fallback are now implemented and tested through real HTTPS; see
[TLS verification](provider-tls-verification.md). Per-provider custom SSL contexts,
installed-opener policy, platform default roots, and complete Path.expanduser
behavior remain native transport work. URL normalization, non-HTTP redirects,
and all urllib timeout edge cases remain compatibility work.
