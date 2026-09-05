# Provider model-list CA policy

The native model-list hook now reads CA bundle settings in Python's order:
`HERMES_CA_BUNDLE`, `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, then `CURL_CA_BUNDLE`.
The first nonblank setting wins. A missing, empty, or malformed bundle falls back
to default client trust rather than trying another environment setting.

Valid PEM bundles replace the built-in trust roots, matching Python's explicit
`ssl.create_default_context(cafile=...)` branch. Certificate loading uses the
[reqwest PEM bundle API](https://docs.rs/reqwest/latest/reqwest/tls/struct.Certificate.html),
with client construction validating the certificates before the custom client is
accepted. Redirect policy and connect/read timeouts remain intact.

## Evidence

Inline tests exercise the public fetch method with real environment settings and
a local tokio-rustls HTTPS server. They verify trusted-CA success, unrelated-CA
rejection, a two-certificate bundle with the needed CA second, hostname rejection,
missing/malformed bundle fallback, and first-variable precedence. A bundle with
both a valid certificate and invalid DER must not leave a partially trusted store.
Environment changes use the repository's global test lock and restore prior values.

The fixture generator was executed and the regenerated files passed the tests.
OpenSSL is a regeneration dependency only; Rust tests embed the local certificates
and key. tokio-rustls was already in the lockfile and is now an explicit development
dependency for this real TLS test.

Workspace: 1,078 passed, one existing bridge test ignored. Formatting, Clippy with
warnings denied, generator shell syntax, and diff whitespace checks pass.
Logs: `provider-tls-workspace-tests.log`, `provider-tls-clippy.log`,
`provider-tls-tests.log`. Gemini source audit: `provider-ca-source-review.md`.

## Remaining scope

Standard `~`/`~/` expansion uses HOME. Named-user expansion, passwd fallback when
HOME is absent, and Windows home selection still differ from Path.expanduser.
Python's installed-opener behavior, platform-specific default certificate stores,
macOS certifi fallback, and explicit per-provider SSL contexts still need native
transport integration. No equivalence across those cases is claimed here.
Provider discovery and provider-specific hooks remain open.
