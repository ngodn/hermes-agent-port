# Local TLS fixtures

Synthetic certificates and a server key for the inline provider model-list tests.
The server certificate has only the DNS name `localhost`, allowing the test to
check hostname rejection through `127.0.0.1` using the same trusted chain.

- `ca.pem`: issuer trusted by successful requests.
- `other-ca.pem`: unrelated issuer used for rejection and multi-certificate tests.
- `server.der`, `server-key.der`: local test server identity.

Regenerate with `bash rust/tools/gen_tls_fixtures.sh`, then run
`cargo test --manifest-path rust/Cargo.toml provider_registry::tests`.
Certificates last ten years from generation. The generator uses temporary CA
keys and removes them on exit. OpenSSL is needed only to regenerate fixtures;
the Rust tests use tokio-rustls and the embedded DER files directly.
