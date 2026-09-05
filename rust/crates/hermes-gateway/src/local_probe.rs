//! Local model-server HTTP detection and Ollama vision probing.
//!
//! Port of the endpoint-probe half of `agent/model_metadata.py`:
//! `detect_local_server_type` and `query_ollama_supports_vision`, plus the
//! normalizers, the connect-timeout blackhole cache, and the L2 disk cache
//! those two functions lean on. See `rust/analysis/live-capability-plan.md`
//! for the corrections this follows (the Python docstring's "process lifetime"
//! claim is wrong: positive verdicts are 3,600s, negatives 300s).
//!
//! The caller supplies the gateway home and reuses the prober across turns.
//! Endpoint locality, provider-prefix resolution, managed runtime capability,
//! and the full catalog lookup remain separate runner dependencies.
#![allow(dead_code)]

use crate::python_value::decimal_digit;
use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// Positive verdict lifetime in the in-memory cache. A recognized server type
/// is trusted for an hour before the waterfall runs again (a server swap on the
/// same port is picked up once this expires).
const PROBE_TTL: Duration = Duration::from_secs(3600);
/// Negative verdict lifetime in the in-memory cache. A "nothing answered"
/// result is only held for five minutes so a server that was still starting up,
/// or a key that was being fixed, recovers within minutes instead of pinning
/// "undetected" for a whole hour (#89863).
const PROBE_FAILURE_TTL: Duration = Duration::from_secs(300);
/// Positive disk-cache lifetime. Cross-process (back-to-back CLI runs, cron
/// ticks) share a successful verdict for five minutes. Negatives are never
/// written to disk.
const DISK_TTL_SECS: f64 = 300.0;
/// How long a single observed connect timeout suppresses further probes to the
/// same host:port. Long enough to collapse one startup's burst, short enough
/// that bringing a VPN up mid-session is picked up without a restart.
const BLACKHOLE_TTL: Duration = Duration::from_secs(30);
/// Per-leg timeout for the detection waterfall.
const DETECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Timeout for the Ollama `/api/show` vision probe.
const SHOW_TIMEOUT: Duration = Duration::from_secs(3);

/// Reuse one prober for a gateway home so successive turns share cached
/// endpoint verdicts. Credentials are supplied separately on each request.
pub struct LocalProbe {
    /// Home directory whose `cache/local_endpoint_probes.json` holds the L2
    /// disk cache. Supplied explicitly, never derived from env here.
    home: PathBuf,
    /// Detection and show use different connect/read timeouts. A failed client
    /// construction yields unknown capability instead of changing HTTP policy.
    client: Option<reqwest::Client>,
    show_client: Option<reqwest::Client>,
    /// server_url -> (verdict, observed_at). `verdict == None` is a cached
    /// negative. Keyed on the normalized, `/v1`-stripped server URL so
    /// `localhost` and `127.0.0.1` (and `/v1`-suffixed or not) share an entry.
    memory: Mutex<HashMap<String, (Option<String>, Instant)>>,
    /// host:port -> last observed connect-timeout instant.
    blackhole: Mutex<HashMap<String, Instant>>,
}

#[cfg(test)]
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

impl LocalProbe {
    /// Build a prober rooted at `home` for its disk cache.
    pub fn new(home: PathBuf) -> Self {
        // HTTPX does not follow redirects and times connect/read separately.
        let client = probe_client(DETECT_TIMEOUT);
        let show_client = probe_client(SHOW_TIMEOUT);
        Self {
            home,
            client,
            show_client,
            memory: Mutex::new(HashMap::new()),
            blackhole: Mutex::new(HashMap::new()),
        }
    }

    /// Detect which local server answers at `base_url`.
    ///
    /// Returns `Some("ollama" | "lm-studio" | "vllm" | "llamacpp")` or `None`
    /// when nothing recognizable answered. Probe order and response predicates
    /// match the Python waterfall exactly: LM Studio `/api/v1/models` (200 is
    /// sufficient), Ollama `/api/tags` (200 AND Python-style `models` membership), llama.cpp
    /// `/v1/props` then `/props` on non-200 (200 AND `default_generation_settings`
    /// in the body), vLLM `/version` (200 AND Python-style `version` membership).
    pub async fn detect_local_server_type(&self, base_url: &str, api_key: &str) -> Option<String> {
        // Resolve localhost to IPv4 on the *normalized* URL before deriving the
        // server / LM Studio roots and before the cache lookup, so localhost and
        // 127.0.0.1 share one cache entry and skip the ~2s IPv6 connect penalty
        // on dual-stack machines.
        let normalized = localhost_to_ipv4(&normalize_base_url(base_url));

        let server_url = strip_v1_suffix(&normalized);
        let lmstudio_url = lmstudio_server_root(&normalized);

        if let Some(verdict) = self.memory_get(&server_url) {
            return verdict;
        }

        // A recently blackholed host: skip the waterfall (each leg would burn
        // its full connect timeout). Deliberately not written to the memory
        // cache, whose hour-long positive entry would pin "undetected".
        if self.is_blackholed(&server_url) {
            return None;
        }

        // Disk L2: a fresh cross-process verdict skips the HTTP waterfall
        // entirely. Only string (successful) verdicts are ever stored.
        if let Some(Value::String(hit)) = self.disk_get("server_type", &server_url) {
            self.memory_put(&server_url, Some(hit.clone()));
            return Some(hit);
        }

        let result = self
            .run_waterfall(&server_url, &lmstudio_url, api_key)
            .await;

        if let Some(ref kind) = result {
            self.memory_put(&server_url, Some(kind.clone()));
            self.disk_put("server_type", &server_url, Value::String(kind.clone()));
        } else {
            // Negative verdict: memory only (short TTL). Never persisted. A
            // failure is usually transient (server starting, key being fixed),
            // but caching it in memory stops the very next turn from re-running
            // the whole waterfall against an endpoint that just went quiet.
            self.memory_put(&server_url, None);
        }
        result
    }

    /// Run the four-leg detection waterfall. No memory lock is held across any
    /// network call. Aborts early (returning `None`) the moment a leg reports a
    /// connect timeout, after recording the blackhole, so the remaining legs
    /// don't each stall out their own timeout.
    async fn run_waterfall(
        &self,
        server_url: &str,
        lmstudio_url: &str,
        api_key: &str,
    ) -> Option<String> {
        let token = auth_token(api_key);

        // LM Studio exposes /api/v1/models, checked first (most specific).
        // HTTP 200 alone is sufficient here.
        match self
            .get(&format!("{lmstudio_url}/api/v1/models"), &token)
            .await
        {
            ProbeResult::Response(200, _, _) => {
                return Some("lm-studio".to_string());
            }
            ProbeResult::ConnectTimeout => {
                self.note_blackholed(server_url);
                return None;
            }
            _ => {}
        }

        // Ollama exposes /api/tags and answers {"models": [...]}. LM Studio
        // returns {"error": ...} with status 200 on this path, so the presence
        // of a "models" key must be verified, not just the 200.
        match self.get(&format!("{server_url}/api/tags"), &token).await {
            ProbeResult::Response(200, _, Some(json)) if json_contains(&json, "models") => {
                return Some("ollama".to_string());
            }
            ProbeResult::ConnectTimeout => {
                self.note_blackholed(server_url);
                return None;
            }
            _ => {}
        }

        // llama.cpp exposes /v1/props (older builds used /props). Body must
        // contain "default_generation_settings". A non-connect error on the
        // first GET ends the leg without trying /props, matching the Python
        // exception flow (only a connect timeout aborts the whole waterfall).
        match self.get(&format!("{server_url}/v1/props"), &token).await {
            ProbeResult::ConnectTimeout => {
                self.note_blackholed(server_url);
                return None;
            }
            ProbeResult::Error => {}
            ProbeResult::Response(first_status, first_body, _) => {
                let (status, body) = if first_status != 200 {
                    // Fallback for older builds.
                    match self.get(&format!("{server_url}/props"), &token).await {
                        ProbeResult::Response(s, b, _) => (s, b),
                        ProbeResult::ConnectTimeout => {
                            self.note_blackholed(server_url);
                            return None;
                        }
                        ProbeResult::Error => (0, String::new()),
                    }
                } else {
                    (first_status, first_body)
                };
                if status == 200 && body.contains("default_generation_settings") {
                    return Some("llamacpp".to_string());
                }
            }
        }

        // vLLM: /version answers {"version": ...}.
        match self.get(&format!("{server_url}/version"), &token).await {
            ProbeResult::Response(200, _, Some(json)) if json_contains(&json, "version") => {
                return Some("vllm".to_string());
            }
            ProbeResult::ConnectTimeout => {
                self.note_blackholed(server_url);
                return None;
            }
            _ => {}
        }

        None
    }

    /// Return `Some(true/false)` when Ollama `/api/show` reports vision support
    /// for `bare_model`, else `None` (server unreachable, not Ollama, or model
    /// unknown).
    ///
    /// `bare_model` MUST already be the normalized bare model name. The Python
    /// original strips a recognized provider prefix via `_strip_provider_prefix`,
    /// which consults the live provider-profile registry and the Ollama
    /// `model:tag` pattern. That stripping stays the CALLER's responsibility:
    /// this module deliberately does not carry a static provider registry (per
    /// the plan's "do not replace it with unconditional splitting on a colon").
    /// Pass the value you'd get from that stripping step.
    ///
    /// Vision resolution order (preserved from Python): a non-empty
    /// `capabilities` list containing "vision" -> true; a non-empty
    /// `capabilities` list WITHOUT "vision" -> false (this beats `model_info`);
    /// otherwise a `model_info` key containing "vision.block_count" -> true;
    /// otherwise `None`.
    pub async fn query_ollama_supports_vision(
        &self,
        bare_model: &str,
        base_url: &str,
        api_key: &str,
    ) -> Option<bool> {
        if bare_model.is_empty() || base_url.is_empty() {
            return None;
        }

        if self
            .detect_local_server_type(base_url, api_key)
            .await
            .as_deref()
            != Some("ollama")
        {
            return None;
        }

        let server_url = strip_v1_suffix(&localhost_to_ipv4(base_url.trim_end_matches('/')));
        let token = auth_token(api_key);

        let json = match self
            .post_json(
                &format!("{server_url}/api/show"),
                &token,
                &serde_json::json!({ "name": bare_model }),
            )
            .await
        {
            Some((200, json)) => json?,
            _ => return None,
        };

        if let Some(Value::Array(caps)) = json.get("capabilities") {
            if caps
                .iter()
                .any(|cap| value_as_lower_string(cap).as_deref() == Some("vision"))
            {
                return Some(true);
            }
            if !caps.is_empty() {
                // A non-empty capabilities list without "vision" is a definite
                // negative that wins over the legacy model_info fallback.
                return Some(false);
            }
        }

        if let Some(Value::Object(model_info)) = json.get("model_info") {
            for key in model_info.keys() {
                if key.to_lowercase().contains("vision.block_count") {
                    return Some(true);
                }
            }
        }

        None
    }

    /// Gate for the auxiliary-vision router: `true` when the active provider
    /// likely fronts a *local* Ollama server worth fingerprinting. Port of
    /// `agent/image_routing.py::_should_probe_ollama_vision`.
    ///
    /// `provider == "ollama"` (case/space-insensitive) short-circuits to `true`
    /// without any network I/O. Otherwise an empty `base_url` is `false`, a
    /// non-local `base_url` is `false` (remote OpenAI-compatible APIs must never
    /// be probed: they can misidentify and, without a key, spray 401s, #89863),
    /// and only a local endpoint proceeds to the shared detection waterfall,
    /// which must report `ollama` for the gate to pass. The `api_key` is
    /// forwarded so a key-protected local endpoint isn't misread as silent.
    pub async fn should_probe_ollama_vision(
        &self,
        provider: &str,
        base_url: &str,
        api_key: &str,
    ) -> bool {
        let p = provider.trim_matches(python_whitespace).to_lowercase();
        if p == "ollama" {
            return true;
        }
        if base_url.is_empty() {
            return false;
        }
        // p != "ollama" here, so the locality gate always applies.
        if !is_local_endpoint(base_url) {
            return false;
        }
        self.detect_local_server_type(base_url, api_key)
            .await
            .as_deref()
            == Some("ollama")
    }

    // ── in-memory verdict cache ──────────────────────────────────────────────

    /// Return a fresh cached verdict for `server_url`, or `None` if absent or
    /// stale. Positive verdicts use the hour TTL, negatives the five-minute one.
    /// A stale entry is left in place (it will be overwritten by the next probe).
    fn memory_get(&self, server_url: &str) -> Option<Option<String>> {
        let guard = self.memory.lock().unwrap();
        let (verdict, at) = guard.get(server_url)?;
        let ttl = if verdict.is_some() {
            PROBE_TTL
        } else {
            PROBE_FAILURE_TTL
        };
        if at.elapsed() < ttl {
            Some(verdict.clone())
        } else {
            None
        }
    }

    fn memory_put(&self, server_url: &str, verdict: Option<String>) {
        self.memory
            .lock()
            .unwrap()
            .insert(server_url.to_string(), (verdict, Instant::now()));
    }

    // ── connect-timeout blackhole ────────────────────────────────────────────

    /// Record that a probe to `base_url` timed out during TCP connect. Pure
    /// bookkeeping; no network I/O, so it adds no probe for callers to mock and
    /// can only fire after a real timeout was already paid.
    fn note_blackholed(&self, base_url: &str) {
        if let Some(key) = endpoint_host_key(base_url) {
            self.blackhole.lock().unwrap().insert(key, Instant::now());
        }
    }

    /// True if a recent probe to `base_url` timed out during TCP connect. Pure
    /// cache lookup; expired entries are dropped on read.
    fn is_blackholed(&self, base_url: &str) -> bool {
        let key = match endpoint_host_key(base_url) {
            Some(key) => key,
            None => return false,
        };
        let mut guard = self.blackhole.lock().unwrap();
        match guard.get(&key) {
            Some(seen) if seen.elapsed() < BLACKHOLE_TTL => true,
            Some(_) => {
                guard.remove(&key);
                false
            }
            None => false,
        }
    }

    // ── L2 disk cache (cache/local_endpoint_probes.json) ─────────────────────

    fn disk_cache_path(&self) -> PathBuf {
        self.home.join("cache").join("local_endpoint_probes.json")
    }

    fn load_disk_cache(&self) -> Map<String, Value> {
        match std::fs::read(self.disk_cache_path()) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(Value::Object(map)) => map,
                _ => Map::new(),
            },
            Err(_) => Map::new(),
        }
    }

    /// Return a still-fresh cached value for `kind:key`, else `None`.
    fn disk_get(&self, kind: &str, key: &str) -> Option<Value> {
        let data = self.load_disk_cache();
        let entry = data.get(&format!("{kind}:{key}"))?;
        let entry = entry.as_object()?;
        let ts = python_float(entry.get("ts")?)?;
        if now_secs() - ts >= DISK_TTL_SECS {
            return None;
        }
        entry.get("value").cloned()
    }

    /// Persist a successful probe result, pruning stale siblings. Best-effort:
    /// any failure is swallowed (the caches above still function without disk).
    fn disk_put(&self, kind: &str, key: &str, value: Value) {
        let now = now_secs();
        let data = self.load_disk_cache();
        let mut kept = Map::new();
        for (key, entry) in data {
            if let Some(object) = entry.as_object() {
                let Some(ts) = python_float(object.get("ts").unwrap_or(&Value::from(0))) else {
                    return;
                };
                if now - ts < DISK_TTL_SECS {
                    kept.insert(key, entry);
                }
            }
        }
        let mut data = kept;
        let mut entry = Map::new();
        entry.insert("value".to_string(), value);
        entry.insert("ts".to_string(), Value::from(now));
        data.insert(format!("{kind}:{key}"), Value::Object(entry));
        let _ = atomic_json_write(&self.disk_cache_path(), &Value::Object(data));
    }

    // ── HTTP helpers ─────────────────────────────────────────────────────────

    async fn get(&self, url: &str, token: &Option<String>) -> ProbeResult {
        let Some(client) = &self.client else {
            return ProbeResult::Error;
        };
        let mut req = client.get(url);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = match resp.text().await {
                    Ok(body) => body,
                    Err(_) => return ProbeResult::Error,
                };
                let json = serde_json::from_str::<Value>(&body).ok();
                ProbeResult::Response(status, body, json)
            }
            Err(err) => {
                if is_connect_timeout(&err) {
                    ProbeResult::ConnectTimeout
                } else {
                    ProbeResult::Error
                }
            }
        }
    }

    /// POST JSON, returning `(status, parsed_body)`. `None` on any transport
    /// error. Vision probing does not blackhole on failure (it only runs once
    /// detection already succeeded), so connect timeouts are not distinguished.
    async fn post_json(
        &self,
        url: &str,
        token: &Option<String>,
        body: &Value,
    ) -> Option<(u16, Option<Value>)> {
        let mut req = self.show_client.as_ref()?.post(url).json(body);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.ok()?;
        let status = resp.status().as_u16();
        let text = resp.text().await.ok()?;
        Some((status, serde_json::from_str::<Value>(&text).ok()))
    }
}

// A failed client initialization behaves like Python's caught client error.
fn probe_client(timeout: Duration) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()
}

fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

// Python's membership operator accepts objects, arrays, and strings here.
fn json_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(needle),
        Value::Array(values) => values.iter().any(|value| value.as_str() == Some(needle)),
        Value::String(value) => value.contains(needle),
        _ => false,
    }
}

fn python_float(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        Value::String(value) => value.trim_matches(python_whitespace).parse().ok(),
        value => value.as_f64(),
    }
}

/// Outcome of one probe leg. `Response` carries the raw body (for the
/// llama.cpp substring check) and its parsed JSON (for the key checks).
enum ProbeResult {
    Response(u16, String, Option<Value>),
    ConnectTimeout,
    Error,
}

// ── free functions: normalizers and small helpers ───────────────────────────

/// `(base_url or "").strip().rstrip("/")`.
fn normalize_base_url(base_url: &str) -> String {
    base_url
        .trim_matches(python_whitespace)
        .trim_end_matches('/')
        .to_string()
}

/// Bearer token if `api_key` is non-empty after trimming, else `None`.
fn auth_token(api_key: &str) -> Option<String> {
    let token = api_key.trim_matches(python_whitespace);
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

// ── endpoint locality (port of model_metadata.is_local_endpoint) ─────────────

/// Local-machine DNS names that `is_local_endpoint` treats as local outright.
const LOCAL_HOSTS: [&str; 4] = ["localhost", "127.0.0.1", "::1", "0.0.0.0"];
/// Docker / Podman / Lima internal DNS suffixes (exactly these three, matching
/// the Python `_CONTAINER_LOCAL_SUFFIXES` tuple).
const CONTAINER_LOCAL_SUFFIXES: [&str; 3] =
    [".docker.internal", ".containers.internal", ".lima.internal"];

/// Return `true` when `base_url` points at the local machine.
///
/// Faithful port of `agent/model_metadata.is_local_endpoint`, preserving its
/// quirks exactly: the unqualified-host (no-dot) shortcut fires *before* any IP
/// parsing, so a bracketed public IPv6 with no embedded IPv4 (e.g.
/// `http://[2606:4700::1111]`) is reported local; the container suffix list is
/// exactly three entries; and a malformed four-component host whose first octet
/// parses to a private prefix (e.g. octal-looking `010.0.0.1`) is accepted via
/// the permissive integer fallback even though strict IP parsing rejects it.
///
/// Locality recognizes loopback, RFC-1918 private ranges, link-local, and
/// Tailscale CGNAT (`100.64.0.0/10`), matching Python's `ipaddress` semantics
/// for 3.12 (including the IPv4-mapped IPv6 rule and the two `192.0.0.9/10`
/// private-range exceptions).
///
pub fn is_local_endpoint(base_url: &str) -> bool {
    let normalized = normalize_base_url(base_url);
    if normalized.is_empty() {
        return false;
    }
    let url = if normalized.contains("://") {
        normalized
    } else {
        format!("http://{normalized}")
    };
    // Any parse error (bracket mismatch, etc.) or empty host maps to the same
    // `false` as Python's `except` / empty-hostname path.
    let host = urlparse_hostname(&url);

    if LOCAL_HOSTS.contains(&host.as_str()) {
        return true;
    }
    if CONTAINER_LOCAL_SUFFIXES
        .into_iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return true;
    }
    // Unqualified hostnames (no dots) are local by definition. This runs before
    // IP parsing, so a dot-free IPv6 literal lands here regardless of its scope.
    if !host.is_empty() && !host.contains('.') {
        return true;
    }
    let address = if host.contains(':') {
        host.split_once('%')
            .map_or(host.as_str(), |(address, _)| address)
    } else {
        host.as_str()
    };
    if let Ok(addr) = address.parse::<std::net::IpAddr>() {
        if addr_is_local(addr) {
            return true;
        }
    }
    // Permissive fallback for a bare four-component host that failed strict IP
    // parsing but still looks like a private prefix (e.g. WSL 172.x, octal
    // octets, or Tailscale CGNAT).
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 {
        if let (Some(first), Some(second)) = (py_int(parts[0]), py_int(parts[1])) {
            if first == 10
                || (first == 172 && (16..=31).contains(&second))
                || (first == 192 && second == 168)
                || (first == 100 && (64..=127).contains(&second))
            {
                return true;
            }
        }
    }
    false
}

/// Extract the hostname the way Python's `urllib.parse.urlparse(...).hostname`
/// does, returning `""` for both the "no host" and the "urlparse raises" cases
/// (`is_local_endpoint` folds both into `false`). Reqwest URL normalization is
/// deliberately avoided: it diverges from urllib on IPv4 shorthand and trailing
/// dots, which the caller relies on.
pub(crate) fn urlparse_hostname(url_in: &str) -> String {
    // urlsplit lstrips C0 controls + space, then deletes every tab/CR/LF.
    let lstripped = url_in.trim_start_matches(|c: char| (c as u32) <= 0x20);
    let mut s: String = lstripped
        .chars()
        .filter(|c| *c != '\t' && *c != '\r' && *c != '\n')
        .collect();

    // Strip a leading scheme: first ':' whose prefix starts ASCII-alpha and is
    // all scheme chars. Otherwise the string is left intact (and won't start
    // with "//", yielding an empty host below, exactly as urlsplit does).
    if let Some(i) = s.find(':') {
        if i > 0 {
            let head = &s[..i];
            let first_ok = head.chars().next().map(|c| c.is_ascii_alphabetic());
            if first_ok == Some(true) && head.chars().all(is_scheme_char) {
                s = s[i + 1..].to_string();
            }
        }
    }

    let after = match s.strip_prefix("//") {
        Some(rest) => rest,
        None => return String::new(),
    };
    // netloc runs up to the first of '/', '?', '#'.
    let netloc_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let netloc = &after[..netloc_end];

    // CPython 3.12 / Unicode 15 rejects authority characters whose NFKC
    // expansion introduces a URL delimiter. ASCII delimiters are handled by
    // the parser itself. The oracle enumerates all Unicode code points.
    if netloc.chars().any(|c| {
        matches!(
            c as u32,
            0x2047
                ..=0x2049
                    | 0x2100
                    | 0x2101
                    | 0x2105
                    | 0x2106
                    | 0x2a74
                    | 0xfe13
                    | 0xfe16
                    | 0xfe55
                    | 0xfe56
                    | 0xfe5f
                    | 0xfe6b
                    | 0xff03
                    | 0xff0f
                    | 0xff1a
                    | 0xff1f
                    | 0xff20
        )
    }) {
        return String::new();
    }

    let has_open = netloc.contains('[');
    let has_close = netloc.contains(']');
    if has_open != has_close {
        // urlsplit raises "Invalid IPv6 URL".
        return String::new();
    }

    // hostinfo is the piece after the last '@' (userinfo dropped).
    let hostinfo = match netloc.rfind('@') {
        Some(k) => &netloc[k + 1..],
        None => netloc,
    };

    let hostname_raw: String = if let Some(bracket) = hostinfo.find('[') {
        // No data is allowed before the bracket in the host portion.
        if bracket != 0 {
            return String::new();
        }
        let rest = &hostinfo[1..];
        let close = match rest.find(']') {
            Some(c) => c,
            None => return String::new(),
        };
        let inner = &rest[..close];
        if !bracketed_host_ok(inner) {
            return String::new();
        }
        // After the bracket only a ':'-led port may follow.
        let after_close = &rest[close + 1..];
        if !after_close.is_empty() && !after_close.starts_with(':') {
            return String::new();
        }
        inner.to_string()
    } else {
        // Brackets present in netloc but not in the host portion (i.e. inside
        // userinfo) make urlsplit run `_check_bracketed_host` on a bare name,
        // which always raises. Fold that to an empty host.
        if has_open {
            return String::new();
        }
        match hostinfo.find(':') {
            Some(c) => hostinfo[..c].to_string(),
            None => hostinfo.to_string(),
        }
    };

    if hostname_raw.is_empty() {
        return String::new();
    }
    // The `.hostname` property lowercases everything up to a '%' zone id and
    // leaves the zone as-is.
    match hostname_raw.find('%') {
        Some(p) => format!("{}{}", hostname_raw[..p].to_lowercase(), &hostname_raw[p..]),
        None => hostname_raw.to_lowercase(),
    }
}

/// Characters urllib accepts in a URL scheme.
fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'
}

/// Validate the text inside `[...]` the way `_check_bracketed_host` does: an
/// `IPvFuture` `v<hex>+.<rest>` literal, otherwise a value that parses as IPv6
/// (a bracketed IPv4 or non-IP raises in Python -> `false` here).
fn bracketed_host_ok(inner: &str) -> bool {
    if let Some(rest) = inner.strip_prefix('v') {
        match rest.find('.') {
            Some(dot) => {
                let hex = &rest[..dot];
                let after = &rest[dot + 1..];
                !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) && !after.is_empty()
            }
            None => false,
        }
    } else {
        let address = if let Some((address, scope)) = inner.split_once('%') {
            if scope.is_empty() || scope.contains('%') {
                return false;
            }
            address
        } else {
            inner
        };
        address.parse::<std::net::Ipv6Addr>().is_ok()
    }
}

// Python permits signs, underscores between decimal digits, and large ints.
// Saturation preserves comparisons against the small network-prefix values.
fn py_int(s: &str) -> Option<i64> {
    let s = s.trim_matches(python_whitespace);
    let (negative, digits) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s.strip_prefix('+').unwrap_or(s))
    };
    let mut value = 0i64;
    let mut previous_digit = false;
    let mut count = 0;
    for c in digits.chars() {
        if c == '_' {
            if !previous_digit {
                return None;
            }
            previous_digit = false;
        } else {
            let digit = decimal_digit(c)?;
            count += 1;
            // CPython's default integer-string conversion limit is 4,300.
            if count > 4300 {
                return None;
            }
            value = value.saturating_mul(10).saturating_add(digit);
            previous_digit = true;
        }
    }
    if !previous_digit {
        return None;
    }
    Some(if negative { -value } else { value })
}

/// True when `addr` is local by Python's `ipaddress` semantics: private,
/// loopback, or link-local, plus the IPv4 Tailscale CGNAT block. IPv4-mapped
/// IPv6 addresses defer to their embedded IPv4's privacy (and, like Python's
/// `isinstance(addr, IPv4Address)` guard, do not get the CGNAT check).
fn addr_is_local(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            ipv4_is_private_py(bits) || v4_in(bits, [100, 64, 0, 0], 10)
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                // Python: is_private follows the mapped IPv4; loopback/link-local
                // and the CGNAT check never fire for a mapped address.
                ipv4_is_private_py(u32::from(mapped))
            } else {
                let bits = u128::from(v6);
                // is_private already covers ::1 and fe80::/10, but OR the loopback
                // and link-local predicates too, mirroring the source expression.
                ipv6_is_private_py(bits)
                    || bits == 1
                    || v6_in(bits, [0xfe80, 0, 0, 0, 0, 0, 0, 0], 10)
            }
        }
    }
}

/// `(addr & mask) == (net & mask)` for a `prefix`-bit IPv4 network.
fn v4_in(addr: u32, net_octets: [u8; 4], prefix: u32) -> bool {
    let net = u32::from_be_bytes(net_octets);
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (addr & mask) == (net & mask)
}

/// IPv4 privacy per Python 3.12's `iana-ipv4-special-registry`, including the
/// two `192.0.0.9/10` exceptions carved back out of `192.0.0.0/24`.
fn ipv4_is_private_py(bits: u32) -> bool {
    const EXCEPTIONS: [[u8; 4]; 2] = [[192, 0, 0, 9], [192, 0, 0, 10]];
    for exc in EXCEPTIONS {
        if bits == u32::from_be_bytes(exc) {
            return false;
        }
    }
    const NETS: [([u8; 4], u32); 14] = [
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 0, 170], 31),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([240, 0, 0, 0], 4),
        ([255, 255, 255, 255], 32),
    ];
    NETS.iter().any(|(net, prefix)| v4_in(bits, *net, *prefix))
}

/// `(addr & mask) == (net & mask)` for a `prefix`-bit IPv6 network, `net` given
/// as eight 16-bit segments.
fn v6_in(addr: u128, net_segments: [u16; 8], prefix: u32) -> bool {
    let net = u128::from(std::net::Ipv6Addr::new(
        net_segments[0],
        net_segments[1],
        net_segments[2],
        net_segments[3],
        net_segments[4],
        net_segments[5],
        net_segments[6],
        net_segments[7],
    ));
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    (addr & mask) == (net & mask)
}

/// IPv6 privacy per Python 3.12's `iana-ipv6-special-registry` for a
/// non-mapped address (mapped addresses are handled by the caller). Matches
/// Python's `any(private) and all(not exception)`.
fn ipv6_is_private_py(bits: u128) -> bool {
    const EXCEPTIONS: [([u16; 8], u32); 6] = [
        ([0x2001, 1, 0, 0, 0, 0, 0, 1], 128),
        ([0x2001, 1, 0, 0, 0, 0, 0, 2], 128),
        ([0x2001, 3, 0, 0, 0, 0, 0, 0], 32),
        ([0x2001, 4, 0x112, 0, 0, 0, 0, 0], 48),
        ([0x2001, 0x20, 0, 0, 0, 0, 0, 0], 28),
        ([0x2001, 0x30, 0, 0, 0, 0, 0, 0], 28),
    ];
    for (net, prefix) in EXCEPTIONS {
        if v6_in(bits, net, prefix) {
            return false;
        }
    }
    const NETS: [([u16; 8], u32); 11] = [
        ([0, 0, 0, 0, 0, 0, 0, 1], 128),
        ([0, 0, 0, 0, 0, 0, 0, 0], 128),
        ([0, 0, 0, 0, 0, 0xffff, 0, 0], 96),
        ([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48),
        ([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
        ([0x2001, 0, 0, 0, 0, 0, 0, 0], 23),
        ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
        ([0x2002, 0, 0, 0, 0, 0, 0, 0], 16),
        ([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),
        ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
        ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
    ];
    NETS.iter().any(|(net, prefix)| v6_in(bits, *net, *prefix))
}

/// Rewrite an anchored, lowercase `http(s)://localhost` HOST to `127.0.0.1`.
///
/// Only the URL's own host (anchored right after the scheme) is rewritten, so a
/// non-localhost URL that merely embeds `http://localhost...` in a query or
/// path passes through untouched. Mirrors the Python regex
/// `^(https?://)localhost(?=[:/]|$)` with `count=1` and its case sensitivity.
fn localhost_to_ipv4(url: &str) -> String {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            if let Some(after) = rest.strip_prefix("localhost") {
                if after.is_empty() || after.starts_with(':') || after.starts_with('/') {
                    return format!("{scheme}127.0.0.1{after}");
                }
            }
            // Scheme matched but host is not exactly localhost: no other scheme
            // can match, so stop.
            break;
        }
    }
    url.to_string()
}

/// Drop a trailing `/v1` from an already-normalized URL.
fn strip_v1_suffix(normalized: &str) -> String {
    match normalized.strip_suffix("/v1") {
        Some(root) => root.to_string(),
        None => normalized.to_string(),
    }
}

/// LM Studio server root for native `/api/v1` endpoints: normalize, then strip
/// the first matching `/api/v1`, `/api`, or `/v1` suffix.
fn lmstudio_server_root(base_url: &str) -> String {
    let mut root = normalize_base_url(base_url);
    for suffix in ["/api/v1", "/api", "/v1"] {
        if let Some(stripped) = root.strip_suffix(suffix) {
            root = stripped.trim_end_matches('/').to_string();
            break;
        }
    }
    root
}

/// `host:port` key for the blackhole cache, or `None` when the URL has no host.
/// Keyed on host:port (not the full URL) so every probe path for one server
/// shares a single entry.
fn endpoint_host_key(base_url: &str) -> Option<String> {
    let normalized = normalize_base_url(base_url);
    if normalized.is_empty() {
        return None;
    }
    let with_scheme = if normalized.contains("://") {
        normalized
    } else {
        format!("http://{normalized}")
    };
    let parsed = reqwest::Url::parse(&with_scheme).ok()?;
    let host = parsed.host_str()?.trim_matches(['[', ']']);
    let port = parsed
        .port()
        .filter(|port| *port != 0)
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    Some(format!("{host}:{port}"))
}

/// True for reqwest errors that are connect-phase timeouts. Read timeouts are
/// excluded: those mean the server accepted the connection, the opposite of the
/// blackhole this guards.
fn is_connect_timeout(err: &reqwest::Error) -> bool {
    err.is_timeout() && err.is_connect()
}

// Only JSON strings can stringify to "vision"; other JSON values cannot match.
fn value_as_lower_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.to_lowercase()),
        _ => None,
    }
}

/// Seconds since the Unix epoch (wall clock), used for the disk-cache TTL. The
/// in-memory caches use a monotonic clock instead.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Atomically write `value` as compact JSON to `path`: write to a uniquely
/// named temp file in the same directory, flush+sync, then rename over the
/// target so the file is never observed half-written. The unique temp name
/// keeps two concurrent writers from clobbering each other's staging file.
fn atomic_json_write(path: &std::path::Path, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    crate::atomic_file::write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::Json;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    // ── real local HTTP fixtures ─────────────────────────────────────────────

    /// A spawned axum server on 127.0.0.1:0. Aborts its task on drop. Tests that
    /// need to assert request order/auth own a `Recorder` and wire it into the
    /// router themselves.
    struct TestServer {
        url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    #[derive(Clone)]
    struct Recorder {
        paths: Arc<StdMutex<Vec<String>>>,
        auth: Arc<StdMutex<Vec<Option<String>>>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                paths: Arc::new(StdMutex::new(Vec::new())),
                auth: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    fn record(rec: &Recorder, path: &str, headers: &axum::http::HeaderMap) {
        rec.paths.lock().unwrap().push(path.to_string());
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        rec.auth.lock().unwrap().push(auth);
    }

    async fn spawn(router: axum::Router) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        TestServer {
            url: format!("http://{addr}"),
            handle,
        }
    }

    fn temp_home() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hermes-local-probe-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    struct TempHome(PathBuf);
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── normalizer unit tests ────────────────────────────────────────────────

    #[test]
    fn localhost_rewrite_is_host_anchored() {
        assert_eq!(
            localhost_to_ipv4("http://localhost:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            localhost_to_ipv4("https://localhost/v1"),
            "https://127.0.0.1/v1"
        );
        assert_eq!(localhost_to_ipv4("http://localhost"), "http://127.0.0.1");
        // Embedded in a query, not the host: untouched.
        assert_eq!(
            localhost_to_ipv4("http://example.com/?u=http://localhost:1"),
            "http://example.com/?u=http://localhost:1"
        );
        // A longer host that merely starts with "localhost": untouched.
        assert_eq!(
            localhost_to_ipv4("http://localhostx:1"),
            "http://localhostx:1"
        );
    }

    #[test]
    fn lmstudio_root_strips_first_matching_suffix() {
        assert_eq!(lmstudio_server_root("http://h:1/api/v1"), "http://h:1");
        assert_eq!(lmstudio_server_root("http://h:1/api"), "http://h:1");
        assert_eq!(lmstudio_server_root("http://h:1/v1"), "http://h:1");
        assert_eq!(lmstudio_server_root("http://h:1"), "http://h:1");
    }

    #[test]
    fn host_key_shares_v1_and_root() {
        assert_eq!(
            endpoint_host_key("http://localhost:11434/v1").as_deref(),
            Some("localhost:11434")
        );
        assert_eq!(
            endpoint_host_key("http://localhost:11434").as_deref(),
            Some("localhost:11434")
        );
        assert_eq!(
            endpoint_host_key("https://host").as_deref(),
            Some("host:443")
        );
        assert_eq!(endpoint_host_key("host:8080").as_deref(), Some("host:8080"));
        assert_eq!(endpoint_host_key(""), None);
    }

    // ── detection: order, predicates, auth ───────────────────────────────────

    #[tokio::test]
    async fn detects_ollama_after_trying_lmstudio_first() {
        let rec = Recorder::new();
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(
                    |State(rec): State<Recorder>, h: axum::http::HeaderMap| async move {
                        record(&rec, "/api/v1/models", &h);
                        axum::http::StatusCode::NOT_FOUND
                    },
                ),
            )
            .route(
                "/api/tags",
                get(
                    |State(rec): State<Recorder>, h: axum::http::HeaderMap| async move {
                        record(&rec, "/api/tags", &h);
                        Json(serde_json::json!({ "models": [] }))
                    },
                ),
            )
            .with_state(rec.clone());
        let server = spawn(router).await;

        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        let kind = probe.detect_local_server_type(&server.url, "sekret").await;
        assert_eq!(kind.as_deref(), Some("ollama"));

        let paths = rec.paths.lock().unwrap().clone();
        assert_eq!(paths, vec!["/api/v1/models", "/api/tags"]);
        // Bearer key forwarded on every leg.
        let auth = rec.auth.lock().unwrap().clone();
        assert!(auth.iter().all(|a| a.as_deref() == Some("Bearer sekret")));
    }

    #[tokio::test]
    async fn lmstudio_needs_only_200() {
        let router = axum::Router::new().route(
            "/api/v1/models",
            get(|| async { Json(serde_json::json!({ "data": [] })) }),
        );
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .detect_local_server_type(&server.url, "")
                .await
                .as_deref(),
            Some("lm-studio")
        );
    }

    #[tokio::test]
    async fn ollama_tags_without_models_key_is_not_ollama() {
        // LM Studio answers /api/tags with 200 but an "error" body; must not be
        // classified as Ollama, and nothing else answers -> None.
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(|| async { Json(serde_json::json!({ "error": "Unexpected endpoint" })) }),
            )
            .fallback(|| async { axum::http::StatusCode::NOT_FOUND });
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(probe.detect_local_server_type(&server.url, "").await, None);
    }

    #[tokio::test]
    async fn detects_llamacpp_via_props_fallback() {
        // /v1/props 404s; /props answers with the sentinel body.
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/props",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/props",
                get(|| async { "{\"default_generation_settings\": {}}" }),
            );
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .detect_local_server_type(&server.url, "")
                .await
                .as_deref(),
            Some("llamacpp")
        );
    }

    #[tokio::test]
    async fn detects_vllm_via_version_key() {
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/props",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/props",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/version",
                get(|| async { Json(serde_json::json!({ "version": "0.6.0" })) }),
            );
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .detect_local_server_type(&server.url, "")
                .await
                .as_deref(),
            Some("vllm")
        );
    }

    // ── caching: memory hit and disk reopen ──────────────────────────────────

    #[tokio::test]
    async fn memory_cache_avoids_second_waterfall() {
        let rec = Recorder::new();
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(
                    |State(rec): State<Recorder>, h: axum::http::HeaderMap| async move {
                        record(&rec, "/api/tags", &h);
                        Json(serde_json::json!({ "models": [] }))
                    },
                ),
            )
            .with_state(rec.clone());
        let server = spawn(router).await;

        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .detect_local_server_type(&server.url, "")
                .await
                .as_deref(),
            Some("ollama")
        );
        assert_eq!(
            probe
                .detect_local_server_type(&server.url, "")
                .await
                .as_deref(),
            Some("ollama")
        );
        // /api/tags hit exactly once; the second call served from memory.
        assert_eq!(rec.paths.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn positive_verdict_persists_to_disk_and_reopens() {
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(|| async { Json(serde_json::json!({ "models": [] })) }),
            );
        let server = spawn(router).await;

        let home = TempHome(temp_home());
        {
            let probe = LocalProbe::new(home.0.clone());
            assert_eq!(
                probe
                    .detect_local_server_type(&server.url, "")
                    .await
                    .as_deref(),
                Some("ollama")
            );
        }
        // Disk file exists with the verdict.
        let disk = home.0.join("cache").join("local_endpoint_probes.json");
        assert!(disk.exists());

        // A fresh instance (cold memory) reads the verdict from disk. Kill the
        // server first so a disk miss would force the waterfall and fail.
        drop(server);
        let fresh = LocalProbe::new(home.0.clone());
        assert_eq!(
            fresh
                .detect_local_server_type("http://127.0.0.1:0", "")
                .await,
            None,
            "unrelated endpoint should not read another endpoint's disk entry"
        );
    }

    #[tokio::test]
    async fn disk_reopen_serves_cached_verdict_without_server() {
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(|| async { Json(serde_json::json!({ "models": [] })) }),
            );
        let server = spawn(router).await;
        let url = server.url.clone();

        let home = TempHome(temp_home());
        {
            let probe = LocalProbe::new(home.0.clone());
            assert_eq!(
                probe.detect_local_server_type(&url, "").await.as_deref(),
                Some("ollama")
            );
        }
        drop(server); // server gone; only disk L2 can answer now

        let fresh = LocalProbe::new(home.0.clone());
        assert_eq!(
            fresh.detect_local_server_type(&url, "").await.as_deref(),
            Some("ollama"),
            "fresh instance should serve the positive verdict from disk"
        );
    }

    #[tokio::test]
    async fn negative_verdict_is_not_written_to_disk() {
        let router = axum::Router::new().fallback(|| async { axum::http::StatusCode::NOT_FOUND });
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(probe.detect_local_server_type(&server.url, "").await, None);
        let disk = home.0.join("cache").join("local_endpoint_probes.json");
        assert!(!disk.exists(), "a negative verdict must never touch disk");
    }

    // ── Ollama vision probe ──────────────────────────────────────────────────

    #[derive(Clone)]
    struct ShowState {
        rec: Recorder,
        body: Value,
    }

    fn ollama_router(show_body: Value) -> (axum::Router, Recorder) {
        let rec = Recorder::new();
        let state = ShowState {
            rec: rec.clone(),
            body: show_body,
        };
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/api/tags",
                get(|| async { Json(serde_json::json!({ "models": [] })) }),
            )
            .route(
                "/api/show",
                post(
                    |State(state): State<ShowState>,
                     h: axum::http::HeaderMap,
                     Json(req): Json<Value>| async move {
                        state.rec.paths.lock().unwrap().push(format!(
                            "/api/show:{}",
                            req.get("name").and_then(Value::as_str).unwrap_or("")
                        ));
                        let auth = h
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        state.rec.auth.lock().unwrap().push(auth);
                        Json(state.body.clone())
                    },
                ),
            )
            .with_state(state);
        (router, rec)
    }

    #[tokio::test]
    async fn vision_true_from_capabilities() {
        let (router, rec) =
            ollama_router(serde_json::json!({ "capabilities": ["completion", "vision"] }));
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .query_ollama_supports_vision("llava", &server.url, "k")
                .await,
            Some(true)
        );
        // /api/show received the bare name and the bearer key.
        let paths = rec.paths.lock().unwrap().clone();
        assert!(paths.contains(&"/api/show:llava".to_string()));
        assert!(rec
            .auth
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.as_deref() == Some("Bearer k")));
    }

    #[tokio::test]
    async fn nonempty_capabilities_without_vision_beats_model_info() {
        // capabilities is present and non-empty but lacks "vision"; a
        // model_info vision.block_count key must NOT flip this to true.
        let (router, _rec) = ollama_router(serde_json::json!({
            "capabilities": ["completion"],
            "model_info": { "clip.vision.block_count": 24 },
        }));
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .query_ollama_supports_vision("m", &server.url, "")
                .await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn vision_true_from_legacy_model_info() {
        // No capabilities list at all: fall back to model_info.
        let (router, _rec) = ollama_router(serde_json::json!({
            "model_info": { "clip.vision.block_count": 24 },
        }));
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .query_ollama_supports_vision("m", &server.url, "")
                .await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn vision_unknown_when_not_ollama() {
        // Only LM Studio answers; the vision probe must short-circuit to None
        // without ever POSTing /api/show.
        let router = axum::Router::new().route(
            "/api/v1/models",
            get(|| async { Json(serde_json::json!({ "data": [] })) }),
        );
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe
                .query_ollama_supports_vision("m", &server.url, "")
                .await,
            None
        );
    }

    #[test]
    fn disk_cache_expiry_coercion_and_failed_prune_match_python() {
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        let path = probe.disk_cache_path();
        let data = serde_json::json!({
            "server_type:fresh": {"ts": now_secs().to_string(), "value": "ollama"},
            "server_type:stale": {"ts": now_secs() - DISK_TTL_SECS, "value": "vllm"},
        });
        atomic_json_write(&path, &data).unwrap();
        assert_eq!(
            probe.disk_get("server_type", "fresh"),
            Some(Value::from("ollama"))
        );
        assert_eq!(probe.disk_get("server_type", "stale"), None);
        probe.disk_put("server_type", "new", Value::from("lm-studio"));
        let disk = probe.load_disk_cache();
        assert!(disk.contains_key("server_type:fresh"));
        assert!(!disk.contains_key("server_type:stale"));
        assert!(disk.contains_key("server_type:new"));
        let corrupt = serde_json::json!({"broken": {"ts": "not-a-number"}});
        atomic_json_write(&path, &corrupt).unwrap();
        probe.disk_put("server_type", "new", Value::from("ollama"));
        assert_eq!(Value::Object(probe.load_disk_cache()), corrupt);
    }

    #[tokio::test]
    async fn vision_empty_inputs_return_none() {
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert_eq!(
            probe.query_ollama_supports_vision("", "http://x", "").await,
            None
        );
        assert_eq!(probe.query_ollama_supports_vision("m", "", "").await, None);
    }

    // ── is_local_endpoint parity (mirrors the Python oracle table) ───────────

    #[test]
    fn is_local_endpoint_matches_python_oracle() {
        // (base_url, expected). Values verified against
        // agent/model_metadata.is_local_endpoint on CPython 3.12.13.
        let cases: &[(&str, bool)] = &[
            // Explicit local hosts and container suffixes.
            ("localhost:11434", true),
            ("http://box.docker.internal", true),
            ("http://svc.lima.internal", true),
            ("http://svc.containers.internal", true),
            // Unqualified (no dot) shortcut fires before IP parsing, so even a
            // public bracketed IPv6 with no embedded IPv4 reads as local.
            ("http://myhost", true),
            ("http://[2606:4700::1111]", true),
            ("http://[2001:db8::1]", true),
            ("http://[fe80::1]", true),
            ("http://[fc00::1]", true),
            // Loopback / private / link-local / Tailscale CGNAT.
            ("http://127.0.0.1", true),
            ("http://10.1.2.3", true),
            ("http://192.168.1.5", true),
            ("http://172.20.0.1", true),
            ("http://169.254.1.1", true),
            ("http://100.77.243.5:11434", true),
            // Malformed four-component fallback: strict IP parse fails, but the
            // first octet still parses to a private prefix.
            ("http://010.0.0.1", true),
            // IPv4-mapped IPv6 defers to the embedded IPv4's privacy.
            ("http://[::ffff:192.168.1.1]", true),
            ("http://[::ffff:8.8.8.8]", false),
            // Public / out-of-range / malformed non-local.
            ("http://8.8.8.8", false),
            ("http://256.1.1.1", false),
            ("http://127.0.0.1.", false),
            ("http://192.168.1", false),
            ("http://192.0.0.9", false),  // private-range exception
            ("http://:8080", false),      // empty host
            ("http://[not valid", false), // bracket mismatch -> urlparse raises
            ("", false),
        ];
        for (url, expected) in cases {
            assert_eq!(
                is_local_endpoint(url),
                *expected,
                "is_local_endpoint({url:?})"
            );
        }
    }

    // ── should_probe_ollama_vision: provider short-circuit and remote gate ───

    #[tokio::test]
    async fn should_probe_true_for_ollama_provider_without_network() {
        // Provider "ollama" short-circuits to true; base_url points nowhere and
        // must never be dialed.
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert!(
            probe
                .should_probe_ollama_vision(" Ollama ", "http://127.0.0.1:0", "")
                .await
        );
    }

    #[tokio::test]
    async fn should_probe_false_for_remote_endpoint_without_probing() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let router = axum::Router::new()
            .route(
                "/api/v1/models",
                get(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "models"
                }),
            )
            .with_state(calls.clone());
        let server = spawn(router).await;
        let address: std::net::SocketAddr =
            server.url.strip_prefix("http://").unwrap().parse().unwrap();
        let url = format!("http://remote.example:{}/v1", address.port());
        let home = TempHome(temp_home());
        let mut probe = LocalProbe::new(home.0.clone());
        probe.client = Some(
            reqwest::Client::builder()
                .no_proxy()
                .resolve("remote.example", address)
                .connect_timeout(DETECT_TIMEOUT)
                .read_timeout(DETECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        );
        assert!(!probe.should_probe_ollama_vision("custom", &url, "").await);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // The same transport can reach the server, proving that the gate
        // skipped I/O rather than merely receiving a network failure.
        assert_eq!(
            probe.detect_local_server_type(&url, "").await.as_deref(),
            Some("lm-studio")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn should_probe_true_for_local_ollama_over_http() {
        // A real local server that fingerprints as Ollama: a non-ollama provider
        // still passes the gate because the endpoint is local and detected.
        let (router, _rec) = ollama_router(serde_json::json!({ "capabilities": [] }));
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert!(
            probe
                .should_probe_ollama_vision("custom", &server.url, "k")
                .await
        );
    }

    #[tokio::test]
    async fn should_probe_false_for_local_non_ollama_over_http() {
        // Local endpoint, but it fingerprints as LM Studio, not Ollama.
        let router = axum::Router::new().route(
            "/api/v1/models",
            get(|| async { Json(serde_json::json!({ "data": [] })) }),
        );
        let server = spawn(router).await;
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert!(
            !probe
                .should_probe_ollama_vision("custom", &server.url, "")
                .await
        );
    }

    #[tokio::test]
    async fn should_probe_false_for_empty_base_url() {
        let home = TempHome(temp_home());
        let probe = LocalProbe::new(home.0.clone());
        assert!(!probe.should_probe_ollama_vision("custom", "", "").await);
    }
}

#[cfg(test)]
mod golden_corpus {
    use super::*;
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, Method, StatusCode, Uri},
        response::IntoResponse,
        Router,
    };
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn endpoint_locality_matches_python() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tools/endpoint-locality-goldens.json"
        ))
        .unwrap();
        let failures: Vec<_> = cases
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| {
                is_local_endpoint(case["url"].as_str().unwrap())
                    != case["expected"].as_bool().unwrap()
            })
            .collect();
        assert!(failures.is_empty(), "Locality mismatches: {failures:#?}");
    }

    #[derive(Clone)]
    struct Scenario {
        responses: Value,
        calls: Arc<Mutex<Vec<Value>>>,
    }

    async fn respond(
        State(state): State<Scenario>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let path = uri.path();
        let mut call = json!({"method": method.as_str(), "path": path, "auth": headers.get("authorization").and_then(|v| v.to_str().ok())});
        if !body.is_empty() {
            call["body"] = serde_json::from_slice(&body).unwrap();
        }
        state.calls.lock().unwrap().push(call);
        let reply = &state.responses[path];
        let status = StatusCode::from_u16(reply["status"].as_u64().unwrap_or(if reply.is_null() {
            404
        } else {
            200
        }) as u16)
        .unwrap();
        let mut headers = HeaderMap::new();
        if let Some(location) = reply["location"].as_str() {
            headers.insert("location", location.parse().unwrap());
        }
        (
            status,
            headers,
            reply["body"].as_str().unwrap_or("").to_owned(),
        )
    }

    struct Cleanup {
        home: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[tokio::test]
    async fn real_http_matches_python_probe_requests_and_results() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/local-probe-goldens.json")).unwrap();
        for kind in ["detection", "vision"] {
            for case in fixture[kind].as_array().unwrap() {
                let state = Scenario {
                    responses: case["responses"].clone(),
                    calls: Arc::new(Mutex::new(Vec::new())),
                };
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/v1", listener.local_addr().unwrap());
                let router = Router::new().fallback(respond).with_state(state.clone());
                let home = std::env::temp_dir().join(format!(
                    "hermes-probe-oracle-{}-{}",
                    std::process::id(),
                    WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                let _cleanup = Cleanup {
                    home: home.clone(),
                    task: tokio::spawn(async move {
                        axum::serve(listener, router).await.unwrap();
                    }),
                };
                let probe = LocalProbe::new(home);
                let result = if kind == "detection" {
                    json!(probe.detect_local_server_type(&url, " key ").await)
                } else {
                    json!(
                        probe
                            .query_ollama_supports_vision("model:7b", &url, " key ")
                            .await
                    )
                };
                assert_eq!(result, case["expected"], "{case}");
                assert_eq!(json!(*state.calls.lock().unwrap()), case["calls"], "{case}");
            }
        }
    }

    #[test]
    fn normalization_matches_python() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/local-probe-goldens.json")).unwrap();
        for case in fixture["normalization"].as_array().unwrap() {
            let normalized =
                localhost_to_ipv4(&normalize_base_url(case["input"].as_str().unwrap()));
            assert_eq!(strip_v1_suffix(&normalized), case["server"], "{case}");
            assert_eq!(
                lmstudio_server_root(&normalized),
                case["lmstudio"],
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn broken_response_bodies_do_not_select_lmstudio_or_retry_props() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let recorded = paths.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = socket.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|chunk| chunk == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                recorded.lock().unwrap().push(path.clone());
                let response = match path.as_str() {
                    "/api/v1/models" | "/v1/props" => "HTTP/1.1 200 OK\r\nContent-Length: 50\r\nConnection: close\r\n\r\nshort",
                    "/version" => "HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"version\": 1}",
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let home = std::env::temp_dir().join(format!(
            "hermes-probe-body-{}-{}",
            std::process::id(),
            WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _cleanup = Cleanup {
            home: home.clone(),
            task,
        };
        let probe = LocalProbe::new(home);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            probe.detect_local_server_type(&url, ""),
        )
        .await
        .unwrap();
        assert_eq!(result.as_deref(), Some("vllm"));
        assert_eq!(
            *paths.lock().unwrap(),
            ["/api/v1/models", "/api/tags", "/v1/props", "/version"]
        );
        assert!(!probe.is_blackholed(&url));
    }

    #[test]
    fn cache_expiration_and_host_scope_follow_source() {
        let home = std::env::temp_dir().join(format!(
            "hermes-probe-ttl-{}-{}",
            std::process::id(),
            WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = LocalProbe::new(home);
        let url = "http://127.0.0.1:1234";
        let now = Instant::now();
        probe
            .memory
            .lock()
            .unwrap()
            .insert(url.into(), (Some("ollama".into()), now - PROBE_FAILURE_TTL));
        assert_eq!(probe.memory_get(url), Some(Some("ollama".into())));
        probe
            .memory
            .lock()
            .unwrap()
            .insert(url.into(), (None, now - PROBE_FAILURE_TTL));
        assert_eq!(probe.memory_get(url), None);
        probe
            .memory
            .lock()
            .unwrap()
            .insert(url.into(), (Some("ollama".into()), now - PROBE_TTL));
        assert_eq!(probe.memory_get(url), None);
        probe.note_blackholed(url);
        assert!(probe.is_blackholed(&format!("{url}/v1")));
        assert!(!probe.is_blackholed("http://127.0.0.1:1235"));
        probe
            .blackhole
            .lock()
            .unwrap()
            .insert(endpoint_host_key(url).unwrap(), now - BLACKHOLE_TTL);
        assert!(!probe.is_blackholed(url));
        assert!(probe.blackhole.lock().unwrap().is_empty());
    }
}
