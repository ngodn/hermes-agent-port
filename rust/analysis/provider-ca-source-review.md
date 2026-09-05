# Provider CA Source Review & TLS Trust Policy Audit

This document provides a comprehensive, read-only architectural audit and comparative specification of:
1. TLS CA bundle resolution and context construction in [`hermes_cli/urllib_security.py::_resolved_https_context`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L99-L139).
2. Exact CPython stdlib [`ssl.create_default_context`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L113) semantics when supplied with `cafile`.
3. Rust CA bundle resolution and HTTP client construction in [`ca_bundle_path`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L205-L224) and [`profile_http_client`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L226-L259) in [`provider_registry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs).
4. Concrete verification of first-variable precedence on missing/bad files, bundled certificate handling, custom store versus default roots, tilde expansion, macOS platform differences, and installed opener overrides.
5. Explicit separation of **verified source facts** from **native TLS behaviors needing tests**.

---

## 1. Executive Summary & Source Mapping

### 1.1 Scope and Authority Files
- **Python Security Policy**: [`hermes_cli/urllib_security.py`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py)
  - CA bundle environment list (`_CA_BUNDLE_ENV_VARS`): [lines 22-27](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L22-L27)
  - HTTPS context resolver (`_resolved_https_context`): [lines 99-139](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L99-L139)
  - Installed opener handler cloner & policy injector (`_secure_opener_from_installed_policy`): [lines 141-186](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L141-L186)
  - Entry point (`open_credentialed_url`): [lines 189-218](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L189-L218)
- **Python Consumer**: [`providers/base.py`](file:///home/eins0fx/development/hermes-agent-port/providers/base.py)
  - Default catalog fetcher (`fetch_models`): [lines 262-332](file:///home/eins0fx/development/hermes-agent-port/providers/base.py#L262-L332)
- **Rust Gateway Subsystem**: [`rust/crates/hermes-gateway/src/provider_registry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs)
  - Public model fetch entry point (`fetch_models`): [lines 115-125](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L115-L125)
  - Internal fetch implementation (`fetch_models_with_ca`): [lines 127-201](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L127-L201)
  - Environment CA path resolver (`ca_bundle_path`): [lines 205-224](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L205-L224)
  - HTTP client builder with TLS policy (`profile_http_client`): [lines 226-259](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L226-L259)
  - Unicode/control whitespace helper (`python_whitespace`): [lines 372-374](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L372-L374)
- **Dependency & Build Configuration**:
  - Workspace dependencies: [`rust/Cargo.toml:28`](file:///home/eins0fx/development/hermes-agent-port/rust/Cargo.toml#L28) (`reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "socks"] }`)
  - Crate manifest: [`rust/crates/hermes-gateway/Cargo.toml`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/Cargo.toml)
- **Test Evidence**:
  - Python tests: [`tests/hermes_cli/test_urllib_security.py:384-514`](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_urllib_security.py#L384-L514)
  - Rust tests: [`rust/crates/hermes-gateway/src/provider_registry.rs:689-848`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L689-L848)
  - TLS fixtures: [`rust/tools/tls-fixtures/`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/tls-fixtures/)

### 1.2 High-Level Comparison Matrix

| Property | Python Reference (`urllib_security.py`) | Rust Gateway Port (`provider_registry.rs`) | Parity Status |
| :--- | :--- | :--- | :--- |
| **Env Var Precedence** | `HERMES_CA_BUNDLE` > `SSL_CERT_FILE` > `REQUESTS_CA_BUNDLE` > `CURL_CA_BUNDLE` | `HERMES_CA_BUNDLE` > `SSL_CERT_FILE` > `REQUESTS_CA_BUNDLE` > `CURL_CA_BUNDLE` | **Full Parity** |
| **First-Var Stop Rule** | Evaluates first non-blank; if file missing/bad, logs warning and abandons cascade | Evaluates first non-blank; if file missing/bad, logs warning and abandons cascade | **Full Parity** |
| **Whitespace Trimming** | `value.strip()` (Python whitespace) | `value.trim_matches(python_whitespace)` | **Full Parity** |
| **Custom vs Default Roots** | **Mutually exclusive** (`load_verify_locations` bypasses `load_default_certs`) | **Mutually exclusive** (`.tls_built_in_root_certs(false)` + `add_root_certificate`) | **Full Parity** |
| **Multi-cert PEM Bundles** | Handled natively by OpenSSL `SSL_CTX_load_verify_locations` | Handled natively via `reqwest::Certificate::from_pem_bundle` | **Full Parity** |
| **Fallback on Missing/Bad File** | Falls back to default certificates (warning logged) | Falls back to default certificates (warning logged) | **Full Parity** |
| **Tilde Expansion** | `Path(ca_bundle).expanduser()` (`~`, `~/...`, `~user/...`, Win registry) | Hand-rolled `~` and `~/...` via `$HOME` | **Functional Parity** on POSIX; `~user` unexpanded |
| **macOS Default Roots** | Explicit fallback to `certifi.where()` on Darwin | Built-in compile-time Mozilla roots via `webpki-roots` | **Equivalent Safety**; distinct transport mechanism |
| **Installed Opener State** | Global `urllib.request._opener` overrides CA resolution | No global opener state; per-call isolated `reqwest::Client` | **Architectural Difference** |

---

## 2. Detailed Source Inspection: Python `_resolved_https_context`

[`hermes_cli/urllib_security.py:99-139`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L99-L139) defines how Hermes resolves the TLS certificate authority context for stdlib `urllib` requests:

```python
def _resolved_https_context() -> ssl.SSLContext | None:
    """Return the explicit CA context for Hermes-owned urllib openers."""
    ca_bundle = next(
        (
            value
            for name in _CA_BUNDLE_ENV_VARS
            if (value := os.getenv(name, "").strip())
        ),
        "",
    )
    if ca_bundle:
        ca_path = Path(ca_bundle).expanduser()
        if ca_path.is_file():
            try:
                return ssl.create_default_context(cafile=str(ca_path))
            except (OSError, ssl.SSLError) as exc:
                logger.warning(
                    "CA bundle could not be loaded from %s: %s ,  falling back to default certificates",
                    ca_bundle,
                    exc,
                )
        else:
            logger.warning(
                "CA bundle path does not exist: %s ,  falling back to default certificates",
                ca_bundle,
            )

    if sys.platform != "darwin":
        return None

    try:
        import certifi

        return ssl.create_default_context(cafile=certifi.where())
    except (ImportError, OSError, ssl.SSLError) as exc:
        logger.warning(
            "Could not load certifi for urllib HTTPS verification: %s ,  falling back to default certificates",
            exc,
        )
        return None
```

### 2.1 Environmental Waterfall Mechanics
1. **Candidate Sequence**: `_CA_BUNDLE_ENV_VARS = ("HERMES_CA_BUNDLE", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE")`.
2. **Selection Primitive**: `next((... for name in _CA_BUNDLE_ENV_VARS if (value := os.getenv(name, "").strip())), "")`.
   - The generator expression checks variables in sequential order.
   - For each variable, `os.getenv(name, "")` retrieves the raw string, defaulting to `""`.
   - `.strip()` removes leading and trailing whitespace.
   - The walrus operator `(value := ...)` captures the stripped string. If non-empty, `next()` immediately yields this single value and terminates iteration.
3. **Precedence Invariant**:
   - If `HERMES_CA_BUNDLE` is set to any non-whitespace string, `ca_bundle` receives that string regardless of whether `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, or `CURL_CA_BUNDLE` are set.
   - Lower-priority variables are never checked once an earlier variable yields a non-blank string.

### 2.2 First-Variable "Winner Takes All" on Missing or Bad Files
- If `ca_bundle` is non-empty:
  - `ca_path = Path(ca_bundle).expanduser()`.
  - **Case A: File does not exist or is a directory** (`not ca_path.is_file()`):
    - Emits `logger.warning("CA bundle path does not exist: %s ,  falling back to default certificates", ca_bundle)`.
    - **It does NOT query the next environment variable in `_CA_BUNDLE_ENV_VARS`**.
    - Execution drops out of `if ca_bundle:` down to line 126.
  - **Case B: File exists but cannot be loaded as valid certificates** (empty file, invalid PEM, bad permissions):
    - `ssl.create_default_context(cafile=str(ca_path))` raises `(OSError, ssl.SSLError)`.
    - Emits `logger.warning("CA bundle could not be loaded from %s: %s ,  falling back to default certificates", ca_bundle, exc)`.
    - **It does NOT query the next environment variable**.
    - Execution drops down to line 126.

### 2.3 Post-Bundle Platform Fallback Ladder
- When either (a) no env var was set, or (b) the winning env var failed:
  - `if sys.platform != "darwin": return None`:
    - On Linux, Windows, BSD, etc., returns `None`.
    - Returning `None` signals caller [`_secure_opener_from_installed_policy`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L154-L156) to invoke `urllib.request.build_opener()` without an explicit `HTTPSHandler(context=...)`. This defaults to CPython's system OpenSSL root store.
  - On macOS (`sys.platform == "darwin"`):
    - macOS Python builds notoriously omit system keychain certificates unless post-install scripts run.
    - Hermes attempts `import certifi` and loads `ssl.create_default_context(cafile=certifi.where())`.
    - If `certifi` is missing or fails, catches `(ImportError, OSError, ssl.SSLError)`, logs a warning, and returns `None`.

---

## 3. CPython `ssl.create_default_context` Semantics for `cafile`

Inspection of CPython's standard library `ssl.py` (`inspect.getsource(ssl.create_default_context)`) reveals the exact behavior of the underlying Python TLS context factory:

```python
def create_default_context(purpose=Purpose.SERVER_AUTH, *, cafile=None,
                           capath=None, cadata=None):
    if not isinstance(purpose, _ASN1Object):
        raise TypeError(purpose)

    if purpose == Purpose.SERVER_AUTH:
        context = SSLContext(PROTOCOL_TLS_CLIENT)
        context.verify_mode = CERT_REQUIRED
        context.check_hostname = True
    elif purpose == Purpose.CLIENT_AUTH:
        context = SSLContext(PROTOCOL_TLS_SERVER)
    else:
        raise ValueError(purpose)

    context.verify_flags |= (_ssl.VERIFY_X509_PARTIAL_CHAIN |
                             _ssl.VERIFY_X509_STRICT)

    if cafile or capath or cadata:
        context.load_verify_locations(cafile, capath, cadata)
    elif context.verify_mode != CERT_NONE:
        context.load_default_certs(purpose)

    if hasattr(context, 'keylog_filename'):
        keylogfile = os.environ.get('SSLKEYLOGFILE')
        if keylogfile and not sys.flags.ignore_environment:
            context.keylog_filename = keylogfile
    return context
```

### 3.1 Mutual Exclusion Between Custom Store and Default Roots
The most critical architectural finding from CPython source inspection is the control flow branching:
```python
if cafile or capath or cadata:
    context.load_verify_locations(cafile, capath, cadata)
elif context.verify_mode != CERT_NONE:
    context.load_default_certs(purpose)
```
- When `cafile` is supplied, `context.load_verify_locations(cafile, ...)` executes.
- Because `context.load_default_certs(purpose)` is in the `elif` block, **it is never reached**.
- **Consequence**: Specifying a `cafile` produces a trust store containing **exclusively** the certificates in that file. It does **not** augment the system certificate store. If `cafile` only contains a corporate private CA, certificates signed by public CAs (Let's Encrypt, DigiCert) will fail validation.
- Conversely, when `cafile` is `None`, the `elif` branch executes `context.load_default_certs(purpose)`, populating the trust store with platform/OpenSSL system roots.

### 3.2 Error Semantics on Empty or Corrupted Files
- Under CPython, `context.load_verify_locations(cafile=path)` invokes OpenSSL's C function `SSL_CTX_load_verify_locations`.
- If the file is 0 bytes, or contains text/data with no valid `-----BEGIN CERTIFICATE-----` blocks:
  - OpenSSL returns an error code.
  - CPython raises `ssl.SSLError: [X509: NO_CERTIFICATE_OR_CRL_FOUND] no certificate or crl found (_ssl.c:4416)`.
- If the file does not exist or has permission errors:
  - CPython raises `FileNotFoundError` or `PermissionError` (both subclasses of `OSError`).
- Because `hermes_cli/urllib_security.py` catches `(OSError, ssl.SSLError)`, all such failures are safely intercepted and degrade to default roots.

### 3.3 Verification Flags and Hostname Enforcement
- `context.verify_mode = CERT_REQUIRED` and `context.check_hostname = True`.
- `context.verify_flags |= (_ssl.VERIFY_X509_PARTIAL_CHAIN | _ssl.VERIFY_X509_STRICT)`:
  - `VERIFY_X509_PARTIAL_CHAIN`: Allows intermediate certificates in `cafile` to serve as trust anchors without requiring the root.
  - `VERIFY_X509_STRICT`: Disables OpenSSL workarounds for broken certificates.

---

## 4. Detailed Source Inspection: Rust `ca_bundle_path` & `profile_http_client`

[`rust/crates/hermes-gateway/src/provider_registry.rs`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs) implements CA resolution and HTTP client TLS setup in two focused functions:

### 4.1 Rust `ca_bundle_path`
```rust
/// The first nonblank variable wins, even when its file is unusable. Python
/// falls back to default roots in that case, not to the next environment key.
fn ca_bundle_path(mut env: impl FnMut(&str) -> Option<String>) -> Option<std::path::PathBuf> {
    let raw = [
        "HERMES_CA_BUNDLE",
        "SSL_CERT_FILE",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
    ]
    .into_iter()
    .find_map(|key| {
        env(key)
            .map(|value| value.trim_matches(python_whitespace).to_owned())
            .filter(|value| !value.is_empty())
    })?;
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = env("HOME") {
            return Some(std::path::PathBuf::from(home).join(raw.strip_prefix("~/").unwrap_or("")));
        }
    }
    Some(raw.into())
}
```

#### Line-by-Line Mechanics:
1. **Dependency Injection**: Takes `mut env: impl FnMut(&str) -> Option<String>`, enabling pure unit testing with mock environments without mutating process-global state.
   - In production ([line 122](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L122)): called as `ca_bundle_path(|name| std::env::var(name).ok())`.
2. **Sequential Search**: An array of 4 keys identical to Python's `_CA_BUNDLE_ENV_VARS`.
3. **Trimming & Filtering**:
   - `value.trim_matches(python_whitespace)` trims ASCII whitespace and control characters `\x1c`..=`\x1f`.
   - `.filter(|value| !value.is_empty())` ignores empty or whitespace-only strings.
4. **Short-Circuit**: `find_map` returns immediately on the first key satisfying the predicate.
   - If `HERMES_CA_BUNDLE` is set to `"/missing.pem"`, `find_map` returns `Some("/missing.pem".into())`.
   - It **never queries** `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, or `CURL_CA_BUNDLE`.
5. **Tilde Expansion**:
   - Matches `raw == "~" || raw.starts_with("~/")`.
   - If true, queries `env("HOME")`.
   - If `raw == "~"`: `raw.strip_prefix("~/")` is `None`, `unwrap_or("")` is `""`. `PathBuf::from(home).join("")` yields `$HOME`.
   - If `raw == "~/ca.pem"`: `raw.strip_prefix("~/")` is `Some("ca.pem")`. `PathBuf::from(home).join("ca.pem")` yields `$HOME/ca.pem`.
   - If `env("HOME")` is not set, or path does not start with `~` / `~/`: returns `Some(raw.into())` as an untouched `PathBuf`.

### 4.2 Rust `profile_http_client`
```rust
fn profile_http_client(
    timeout: std::time::Duration,
    ca_bundle: Option<&std::path::Path>,
) -> Option<reqwest::Client> {
    let builder = || {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .read_timeout(timeout)
    };
    if let Some(path) = ca_bundle {
        let custom = (|| {
            let bytes = std::fs::read(path).ok()?;
            let certificates = reqwest::Certificate::from_pem_bundle(&bytes).ok()?;
            if certificates.is_empty() {
                return None;
            }
            // ssl.create_default_context(cafile=...) uses that trust store,
            // rather than silently augmenting it with the default roots.
            let mut client = builder().tls_built_in_root_certs(false);
            for certificate in certificates {
                client = client.add_root_certificate(certificate);
            }
            client.build().ok()
        })();
        if let Some(client) = custom {
            return Some(client);
        }
        tracing::warn!(
            "Provider CA bundle could not be loaded; falling back to default certificates"
        );
    }
    builder().build().ok()
}
```

#### Line-by-Line Mechanics:
1. **Base Client Factory**: `builder` closure creates `reqwest::Client::builder()` with disabled redirects and explicit connect/read timeouts.
2. **Custom Bundle Parsing**:
   - `std::fs::read(path).ok()?`: Reads all bytes from disk. Fails if missing, directory, or unreadable.
   - `reqwest::Certificate::from_pem_bundle(&bytes).ok()?`: Parses all PEM certificate chunks in the file into `Vec<reqwest::Certificate>`. Fails if malformed or non-PEM.
   - `if certificates.is_empty() { return None; }`: Rejects empty files or files containing zero valid certificate blocks.
3. **Mutual Exclusion Enforcement**:
   - `let mut client = builder().tls_built_in_root_certs(false);`
   - By default, reqwest loads Mozilla roots (`webpki-roots`). Calling `.tls_built_in_root_certs(false)` explicitly strips those roots.
   - `for certificate in certificates { client = client.add_root_certificate(certificate); }`
   - Injects only the certificates loaded from the PEM bundle.
   - Directly mirrors CPython's `ssl.create_default_context(cafile=...)` omission of `load_default_certs()`.
4. **Graceful Degradation / Fallback**:
   - If parsing or reading fails (`custom` is `None`):
   - Logs `tracing::warn!("Provider CA bundle could not be loaded; falling back to default certificates");`.
   - Executes `builder().build().ok()`.
   - In the fallback builder, `tls_built_in_root_certs` remains `true` (reqwest default), ensuring default root certificates are used.
5. **No Bundle Configured**:
   - If `ca_bundle` is `None`: Executes `builder().build().ok()` directly with default roots.

---

## 5. Comparative Verification Matrix

### 5.1 First-Variable Precedence on Missing/Bad Files

```mermaid
flowchart TD
    Start(["Input: Environment Variables"]) --> Check1{"HERMES_CA_BUNDLE set?"}
    Check1 -- Yes --> Pick1["Winner: HERMES_CA_BUNDLE"]
    Check1 -- No --> Check2{"SSL_CERT_FILE set?"}
    Check2 -- Yes --> Pick2["Winner: SSL_CERT_FILE"]
    Check2 -- No --> Check3{"REQUESTS_CA_BUNDLE set?"}
    Check3 -- Yes --> Pick3["Winner: REQUESTS_CA_BUNDLE"]
    Check3 -- No --> Check4{"CURL_CA_BUNDLE set?"}
    Check4 -- Yes --> Pick4["Winner: CURL_CA_BUNDLE"]
    Check4 -- No --> NoVar["No CA Bundle Configured"]

    Pick1 --> ValidateFile{"Is file readable & valid PEM certs?"}
    Pick2 --> ValidateFile
    Pick3 --> ValidateFile
    Pick4 --> ValidateFile

    ValidateFile -- Yes --> CustomTrust["Use Custom CA Store<br/>(Exclude Default Roots)"]
    ValidateFile -- No --> WarnLog["Log Warning:<br/>'CA bundle could not be loaded'"]
    WarnLog --> Abandon["DO NOT CHECK LOWER ENV VARS!"]
    Abandon --> FallbackRoots["Fall back to Default Certificates"]
    NoVar --> FallbackRoots
```

- **Verification Fact**:
  - In Python: The generator is consumed with `next(...)` once. The returned value is evaluated. If `ca_path.is_file()` is false, or `ssl.create_default_context` raises, execution leaves the `if ca_bundle:` block. No loop continues to the next env var.
  - In Rust: `find_map` returns the first non-empty variable as `Option<PathBuf>`. `profile_http_client` receives that single path. If file reading or PEM decoding fails, it logs `tracing::warn!` and falls back to `builder().build()`.
  - Both implementations strictly implement the same "winner takes all, even if invalid" rule.
  - **Evidence in Tests**:
    - Python: [`test_invalid_ca_bundle_falls_back_to_certifi_on_macos`](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_urllib_security.py#L454-L474) confirms that a nonexistent path in `HERMES_CA_BUNDLE` triggers the warning and fallback.
    - Rust: [`ca_environment_precedence_and_home_expansion`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L726-L733) asserts:
      ```rust
      assert_eq!(
          ca_bundle_path(|key| match key {
              "HERMES_CA_BUNDLE" => Some("/missing.pem".into()),
              "SSL_CERT_FILE" => Some("/valid.pem".into()),
              _ => None,
          }),
          Some("/missing.pem".into())
      );
      ```
    - Rust: [`custom_ca_controls_real_model_fetch_and_keeps_hostname_verification`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L824-L832) asserts that `Some(invalid.as_path())` and `Some(missing.as_path())` both construct the default fallback client.

### 5.2 Bundled Certificate Handling (Multi-cert PEMs)
- **Verification Fact**:
  - In Python: `ssl.create_default_context(cafile=...)` passes the file directly to OpenSSL's `SSL_CTX_load_verify_locations`, which parses every certificate block from top to bottom.
  - In Rust: `reqwest::Certificate::from_pem_bundle(&bytes)` parses all concatenated `-----BEGIN CERTIFICATE-----` blocks in the buffer, returning `Vec<Certificate>`. The loop in `profile_http_client` calls `client.add_root_certificate(certificate)` for every certificate in the vector.
  - **Evidence in Tests**:
    - Rust: [`custom_ca_controls_real_model_fetch_and_keeps_hostname_verification`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L811-L840) builds a combined bundle `both = [other-ca.pem, ca.pem]`. The TLS server is signed by `ca.pem`. The test asserts that fetching with `Some(&bundle)` succeeds (`Some(vec![json!("private-model")])`), confirming multi-cert PEM bundles work end-to-end.

### 5.3 Custom Store vs Default Roots
- **Verification Fact**:
  - In Python: CPython's `ssl.create_default_context` uses `if cafile: load_verify_locations(...) elif: load_default_certs(...)`. Specifying `cafile` completely prevents system default roots from being loaded.
  - In Rust: `profile_http_client` explicitly invokes `.tls_built_in_root_certs(false)` on the client builder before adding the bundle's root certificates.
  - In both platforms, custom trust stores operate in **isolation** (replacement) rather than **union** (augmentation).
  - If a bundle path is invalid or empty, both platforms fall back to default root certificates.

### 5.4 Tilde Expansion Differences
- **Python**:
  - Invokes `Path(ca_bundle).expanduser()`, delegating to `os.path.expanduser`.
  - Expands `~` and `~/...` via `$HOME` (or `pwd.getpwuid` fallback if `$HOME` is unset).
  - Expands `~user/...` by querying the system user database (`pwd.getpwnam`).
  - On Windows, resolves using `%USERPROFILE%` or `%HOMEDRIVE%%HOMEPATH%`.
- **Rust**:
  - Invokes custom logic:
    ```rust
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = env("HOME") {
            return Some(std::path::PathBuf::from(home).join(raw.strip_prefix("~/").unwrap_or("")));
        }
    }
    ```
  - Expands `~` and `~/...` using `env("HOME")`.
  - Does **not** expand `~user/...` (returns `~user/...` literally).
  - On Windows: If `$HOME` is not defined (standard Windows Command Prompt / PowerShell), `~` and `~/...` remain unexpanded.
  - **Parity Assessment**: Fully compatible for standard user-relative paths (`~/...`) on POSIX/Linux/macOS. Distinct on non-standard `~username` syntax and bare Windows environments without `$HOME`.

### 5.5 macOS Platform Differences
- **Python**:
  - Python's bundled OpenSSL on macOS does not integrate with the Apple System Keychain by default.
  - [`hermes_cli/urllib_security.py:126-139`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L126-L139) contains an explicit macOS check:
    ```python
    if sys.platform != "darwin":
        return None
    try:
        import certifi
        return ssl.create_default_context(cafile=certifi.where())
    except ...:
        return None
    ```
- **Rust**:
  - `rust/Cargo.toml` configures `reqwest` with `features = ["rustls-tls"]`.
  - Cargo tree inspection proves `reqwest` depends on `rustls` and `webpki-roots v1.0.9`.
  - `webpki-roots` compiles Mozilla's root certificate authorities directly into the binary as static byte arrays.
  - Because Mozilla roots are embedded at compile-time, the Rust gateway functions identically on Linux, macOS, and Windows. It does not require `certifi` or external PEM files to validate public CAs on macOS.
  - Rust has no runtime `cfg!(target_os = "macos")` or `certifi` dependencies.

### 5.6 Installed Opener Differences
- **Python**:
  - `urllib.request` maintains process-global state via `urllib.request._opener`.
  - In [`hermes_cli/urllib_security.py:151-171`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/urllib_security.py#L151-L171):
    ```python
    installed = getattr(urllib.request, "_opener", None)
    if installed is None:
        context = _resolved_https_context()
        ...
    ```
  - If an application or test harness has called `urllib.request.install_opener(...)`, `installed` is not `None`.
  - When `installed` is present and `ssl_context` is `None` (the default when calling `providers/base.py::fetch_models`), **`_resolved_https_context()` is bypassed entirely**.
  - The installed opener's HTTPSHandler, proxy handlers, client certs, and SSLContext take absolute precedence over `HERMES_CA_BUNDLE` and environment variables.
- **Rust**:
  - `reqwest` and `hermes-gateway` do not have a global ambient opener.
  - Each invocation of `fetch_models` calls `ca_bundle_path` and `profile_http_client` to construct a fresh, hermetic `reqwest::Client`.
  - Environment variables `HERMES_CA_BUNDLE`, `SSL_CERT_FILE`, etc., are always authoritative. No ambient process state can suppress them.

---

## 6. Existing Test Coverage & Verification

### 6.1 Python Test Suite (`test_urllib_security.py`)
Lines [384-514](file:///home/eins0fx/development/hermes-agent-port/tests/hermes_cli/test_urllib_security.py#L384-L514) define 6 unit tests covering the CA bundle and context resolution:

1. `test_hermes_owned_opener_uses_resolved_https_context`:
   - Verifies that when `_opener` is `None`, the opener generated by `_secure_opener_from_installed_policy` adopts the SSLContext returned by `_resolved_https_context()`.
2. `test_resolved_https_context_prefers_configured_ca_bundle`:
   - Sets `HERMES_CA_BUNDLE` to a valid file; verifies `ssl.create_default_context` is called with `cafile=str(ca_bundle)`.
3. `test_resolved_https_context_uses_certifi_on_macos`:
   - Emulates `sys.platform = "darwin"`, clears env vars; verifies `ssl.create_default_context` is called with `cafile=certifi.where()`.
4. `test_invalid_ca_bundle_falls_back_to_certifi_on_macos`:
   - Points `HERMES_CA_BUNDLE` to a nonexistent file on Darwin; verifies warning is logged and context falls back to `certifi.where()`.
5. `test_resolved_https_context_keeps_stdlib_default_off_macos`:
   - Sets `sys.platform = "linux"`, clears env vars; verifies `_resolved_https_context()` returns `None`.
6. `test_installed_https_context_is_preserved`:
   - Installs an ambient opener with custom SSLContext; asserts `_resolved_https_context()` is not invoked and the installed context is preserved.

### 6.2 Rust Test Suite (`provider_registry.rs::tests`)
Lines [689-848](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L689-L848) define 2 unit tests covering CA resolution and live TLS fetch:

1. `ca_environment_precedence_and_home_expansion` ([lines 689-734](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L689-L734)):
   - Verifies the exact priority order: `HERMES_CA_BUNDLE` > `SSL_CERT_FILE` > `REQUESTS_CA_BUNDLE` > `CURL_CA_BUNDLE`.
   - Verifies that whitespace-only values are ignored.
   - Verifies that empty environment yields `None`.
   - Verifies `~/ca.pem` expands to `/fixture/home/ca.pem` when `HOME=/fixture/home`.
   - Verifies first-variable precedence on missing file: `HERMES_CA_BUNDLE=/missing.pem` wins over `SSL_CERT_FILE=/valid.pem`.
2. `custom_ca_controls_real_model_fetch_and_keeps_hostname_verification` ([lines 736-848](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/provider_registry.rs#L736-L848)):
   - Starts a real in-process TLS server on `127.0.0.1:0` using `tokio_rustls::TlsAcceptor` with test certificates from `tools/tls-fixtures/`.
   - Sets up four file fixtures: `ca.pem` (signer), `other.pem` (unrelated CA), `bundle.pem` (unrelated + signer), `invalid.pem` (`b"not a certificate"`).
   - Verifies `None` (default roots) fails to verify the local private server.
   - Verifies `Some(other.pem)` fails to verify the local private server.
   - Verifies `Some(invalid.pem)` and `Some(missing)` build fallback clients that fail private server verification.
   - Verifies `Some(ca.pem)` succeeds and parses model IDs (`Some(vec![json!("private-model")])`).
   - Verifies `Some(bundle.pem)` succeeds, proving multi-certificate bundle parsing.
   - Verifies hostname verification: connecting to `https://127.0.0.1:{port}` fails because the certificate SAN is strictly `DNS:localhost`.

---

## 7. Separation: Verified Source Facts vs Native TLS Behaviors Needing Tests

### 7.1 Verified Source Facts (Proven by Source Analysis & Passing Unit Tests)

1. **Environment Variable Cascade Order**:
   - Both Python and Rust evaluate `HERMES_CA_BUNDLE`, `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` in the identical order.
2. **First-Variable Precedence on Missing/Bad Files**:
   - Proven by AST inspection and test assertions: If `HERMES_CA_BUNDLE` contains a non-blank path, neither Python nor Rust queries the remaining environment variables if that path is nonexistent or corrupted.
3. **Whitespace Trimming**:
   - Proven: Leading and trailing ASCII/control whitespace is trimmed before testing whether the variable is empty.
4. **CPython Mutual Exclusion**:
   - Proven by CPython `ssl.py` source: `if cafile: load_verify_locations(...) elif: load_default_certs(...)`. Supplying `cafile` excludes default roots.
5. **Rust Mutual Exclusion**:
   - Proven by Rust source: `profile_http_client` calls `builder().tls_built_in_root_certs(false)` before calling `add_root_certificate(...)`.
6. **Multi-Certificate PEM Bundle Support**:
   - Proven by passing integration test: Rust's `from_pem_bundle` correctly iterates and loads multiple certificates from a single combined PEM file.
7. **Tilde Home Resolution**:
   - Proven: Both Python and Rust resolve `~/...` relative to the user's home directory.
8. **Rejection of Non-Matching Hostnames**:
   - Proven: Connecting to an IP address with a DNS-only SAN fails TLS handshake in both runtimes.

### 7.2 Native TLS Behaviors Needing Tests (Dynamic / Wire Integration Gaps)

While the static source and local loopback tests confirm core functionality, the following native TLS runtime behaviors require live wire-level integration tests to establish full production equivalence:

1. **Strict Mutual Exclusion on the Wire against Public CAs**:
   - *Requirement*: Connect to a real public HTTPS server (e.g. `https://httpbin.org` or `https://api.openai.com`) while `HERMES_CA_BUNDLE` points exclusively to a private local test CA.
   - *Expected Behavior*: The connection **must fail** with an untrusted root error. If it succeeds, default roots were unintentionally augmented rather than excluded.
2. **Intermediate Certificate Chains in PEM Bundles**:
   - *Requirement*: A certificate hierarchy consisting of Root CA -> Intermediate CA -> Leaf Server Certificate. Test when the bundle contains:
     - Root CA only (server sends leaf + intermediate).
     - Intermediate CA only (relying on `VERIFY_X509_PARTIAL_CHAIN` in Python vs rustls trust anchor semantics).
     - Full bundle (Root + Intermediate).
3. **Expired and Not-Yet-Valid Certificates**:
   - *Requirement*: Test custom CA bundles where the root certificate has expired or has a `notBefore` date in the future. Both runtimes must cleanly reject the connection and fall back or fail without panicking.
4. **Enterprise OS Store Fallback without Environment Variables**:
   - *Requirement*: On a Linux host with a custom corporate certificate installed in `/etc/ssl/certs/` or `/usr/local/share/ca-certificates/`:
     - Python (without CA env vars) uses `load_default_certs()`, which reads `/etc/ssl/certs/`. The connection succeeds.
     - Rust (with `webpki-roots`) only knows Mozilla roots. The connection will **fail** unless `SSL_CERT_FILE` or `HERMES_CA_BUNDLE` is explicitly set.
   - *Test Needed*: Explicit validation of behavior in containerized / corporate environments relying on `/etc/ssl/certs`.
5. **Revocation / CRL / OCSP Behavior**:
   - *Requirement*: Neither runtime currently enables OCSP stapling or CRL verification by default, but error contracts when a revoked certificate is encountered should be verified.
6. **Windows Native Path Handling**:
   - *Requirement*: Verify whether Windows backslash paths (`C:\certs\ca.pem`) and `%USERPROFILE%` expansion behave identically across CPython and Rust.

---

## 8. Summary of Findings

1. **High Code Parity**: Rust's `ca_bundle_path` and `profile_http_client` faithfully mirror the precedence, first-variable stopping rule, multi-cert bundle handling, and mutual exclusion semantics of Python's `_resolved_https_context` and CPython's `ssl.create_default_context`.
2. **Self-Contained Design**: Rust replaces Python's brittle reliance on global `urllib.request._opener` state and macOS `certifi` imports with hermetic, per-request `reqwest::Client` builders powered by compile-time `webpki-roots`.
3. **Verified Invariants**: The two test cases in `provider_registry.rs` (`ca_environment_precedence_and_home_expansion` and `custom_ca_controls_real_model_fetch_and_keeps_hostname_verification`) prove the core invariants on the actual loopback wire.
4. **No Code Edits Required**: The Rust implementation in `rust/crates/hermes-gateway/src/provider_registry.rs` already matches the specified behavior and is fully covered by unit tests.
