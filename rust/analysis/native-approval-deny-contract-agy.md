# Python Contract for Native Terminal Security: `approvals.deny` Matching, `approvals.mode: off`, and Terminal Execution

## 1. Executive Summary and Architectural Decision

### 1.1 Core Decision
A native static deny-rule slice can safely make the terminal eligible when `approvals.mode: off` and `approvals.deny` is nonempty without implementing interactive approvals.

### 1.2 Rationale
In the authoritative Python implementation ([`tools/approval.py:4740-4785`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4740-L4785)), when `approvals.mode` is set to `"off"`, interactive approvals are never invoked for any command:
1. Commands are first checked against the unconditional hardline floor ([`detect_hardline_command`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L754-L790)) and the sudo stdin guessing guard ([`_check_sudo_stdin_guard`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L735-L751)). Both are static, non-interactive rejections.
2. Commands are next evaluated against user deny rules via [`_match_user_deny_rule`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L794-L823). If matched, the command is blocked statically and unconditionally with a structured refusal. No user prompt is ever displayed or queried.
3. If neither check blocks the command, the bypass check at line 4781 evaluates `approval_mode == "off"`. It immediately returns `{"approved": True, "message": None}`.
4. Downstream interactive checks, callback resolution, dangerous-pattern scanning, and gateway ask states are completely bypassed.

Because every outcome under `approvals.mode: off` is purely static (unconditional hardline block, unconditional sudo-stdin block, unconditional user-deny block, or unconditional approval), no interactive approval mechanisms (such as turn suspension, prompt presentation, response routing, or lease release) are required on any execution path.

### 1.3 Scope and Governance
This document owns only the Python contract analysis for `approvals.deny` matching and its execution interaction. It does not alter Rust production code, `PORT.md`, `INDEX.md`, or existing tests. The accompanying source-executed oracle at [`rust/tools/approval-deny-contract-oracle.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/approval-deny-contract-oracle.py) and test corpus at [`rust/tools/approval-deny-contract-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/approval-deny-contract-goldens.json) verify 59 reference cases directly against the Python implementation without executing any candidate shell commands.

---

## 2. Rule Ingestion, Parsing, and Normalization Contract

### 2.1 Configuration Source
User deny rules are configured in `config.yaml` under the `approvals` mapping:
```yaml
approvals:
  mode: "off"
  deny:
    - "git push*"
    - "curl *forbidden*"
    - "rm -rf *"
```

### 2.2 Access Path and Live Reloading
Configuration is fetched through [`_get_approval_config()`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3435-L3448):
```python
def _get_approval_config() -> dict:
    try:
        from hermes_cli.config import load_config_readonly
        config = load_config_readonly()
        return config.get("approvals", {}) or {}
    except Exception as e:
        logger.warning("Failed to load approval config: %s", e)
        return {}
```
[`load_config_readonly()`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py#L3900-L3920) calls [`_load_config_impl(want_deepcopy=False)`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py#L4096-L4145). On every single command execution, this function checks the filesystem modification timestamp (`st_mtime_ns`) and file size (`st_size`) of `config.yaml` under `_CONFIG_LOCK`. Therefore, any disk edit to `config.yaml` is picked up immediately on the subsequent command.

### 2.3 Rule Parsing and Filtering Rules
In [`tools/approval.py:808-817`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L808-L817):
```python
try:
    deny_patterns = _get_approval_config().get("deny") or []
except Exception:
    return None
if not deny_patterns:
    return None
globs = [p.strip() for p in deny_patterns
         if isinstance(p, str) and p.strip()]
if not globs:
    return None
```

Verified parsing behaviors:
1. **Missing or Empty Key**: If `deny` is absent, `None`, an empty list `[]`, or empty dictionary `{}`, the function returns `None` immediately.
2. **Whitespace Stripping**: Each entry is stripped of leading and trailing whitespace using `p.strip()`.
3. **Empty Entry Filtering**: Any entry that becomes empty after stripping (e.g., `""`, `"   "`, `"\t\n"`) is completely discarded.
4. **Type Filtering**: Non-string entries (such as integers, booleans, lists, mappings, or `None`) fail `isinstance(p, str)` and are dropped without throwing an error.
5. **Exception Fail-Open**: If `_get_approval_config()` raises an exception, the outer `try...except` catches it and returns `None` (failing open for deny matching).
6. **First-Match Precedence**: If multiple patterns in `globs` match a candidate command variant, the matcher returns the first pattern according to its index in the `approvals.deny` list.
7. **Casing Preservation in Return Value**: While matching is case-insensitive, the string returned by [`_match_user_deny_rule`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L794) retains the exact casing of the pattern as specified by the user in `config.yaml` (with whitespace stripped).

---

## 3. Command Normalization and Variant Generation Pipeline

### 3.1 Variant Evaluation Loop
[`_match_user_deny_rule`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L818-L823) iterates through all command variants produced by [`_command_detection_variants(command)`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2412-L2486):
```python
for command_variant in _command_detection_variants(command):
    candidate = command_variant.lower().strip()
    for pattern in globs:
        if fnmatch.fnmatchcase(candidate, pattern.lower()):
            return pattern
```

### 3.2 Exact Steps in `_command_detection_variants`
The variant generator applies several defensive transformations in order:

1. **Quoted Newline Masking ([`_mask_quoted_newlines`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2322-L2367))**:
   Newlines occurring inside single quotes (`'...'`) or double quotes (`"..."`, respecting `\"` escapes) are replaced with spaces. This prevents multi-line arguments (such as commit messages or heredocs) from being split into separate command boundaries.

2. **Core Sanitization ([`_normalize_command_for_detection`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L1321-L1379))**:
   - **ANSI Stripping**: Strips ECMA-48 escape sequences using [`tools/ansi_strip.py`](file:///home/eins0fx/development/hermes-agent-port/tools/ansi_strip.py).
   - **Null Byte Removal**: Removes `\x00`.
   - **Unicode Normalization**: Applies `unicodedata.normalize('NFKC', command)`.
   - **Line Continuation Collapsing**: Collapses `re.sub(r'\\\r?\n', '', command)`. A command like `git push \\\n --force` becomes `git push  --force`.
   - **Home Prefix Rewriting**: Rewrites absolute user home paths to `~/` ([`_rewrite_resolved_user_home`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L1362)) and Hermes home paths to `~/.hermes/` ([`_rewrite_resolved_hermes_home`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L1361)).
   - **Backslash Escape Stripping**: Strips shell backslash escapes via `re.sub(r'\\([^\n])', r'\1', command)`. Tokens such as `g\it` or `r\m` become `git` and `rm`.
   - **Empty Quote Removal**: Strips `re.sub(r"''|\"\"", '', command)`. Obfuscations like `git pu""sh` or `r''m` normalize to `git push` and `rm`.
   - **IFS Expansion**: Replaces `$IFS` and `${IFS}` expansions with literal spaces using `re.sub(r'\$\{IFS\b[^}]*\}|\$IFS\b', ' ', command)`.

3. **Grep Safe Variant ([`_grep_safe_detection_variant`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L1757-L1767))**:
   Hides structurally identified pattern arguments in `grep` commands with spaces so search queries do not trip command patterns. This is yielded as the primary variant (`grep_safe`).

4. **Windows Path Flattening ([`tools/approval.py:2434-2440`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2434-L2440))**:
   If the raw command contains drive-letter or UNC backslash paths (`[A-Za-z]:\` or `\\`), backslashes are replaced with forward slashes (`/`) before normalization, and yielded as an additional variant.

5. **Program-Bearing Option Payloads ([`_execution_flag_findings`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L1960-L1995))**:
   For shell execution carriers (`bash -c <payload>`, `sh -c <payload>`, `sh -lc <payload>`, `zsh`, `ksh`), the inner shell payload is extracted and yielded as a standalone variant. Nested carriers are recursively unwrapped.

6. **Mark Command Starts ([`_mark_command_starts`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2294-L2320))**:
   Inserts a newline `\n` before every quote-aware command start offset found by [`_iter_shell_command_starts`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2219-L2292) (after `;`, `\n`, `&&`, `||`, `&`, `|`, `(`, `{`, `$(`, and backticks).

7. **Word Span Deobfuscation ([`_iter_shell_command_word_spans`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2370-L2409))**:
   Scans executable positions and deobfuscates shell words (e.g., `sudo r\m` -> `sudo rm`).

---

## 4. `fnmatch` Behavior and Glob Matching Semantics

### 4.1 Cross-Platform Case Matching
Matching uses:
```python
fnmatch.fnmatchcase(candidate, pattern.lower())
```
Where `candidate = command_variant.lower().strip()`.

Because both the candidate variant and the user pattern are converted to lower-case via `.lower()` and matched using `fnmatchcase` (which performs exact character-by-character comparison), the matching behavior is uniformly case-insensitive on all platforms (Linux, macOS, Windows). It avoids platform-dependent filesystem case folding in standard `fnmatch.fnmatch`.

### 4.2 Glob Syntax Supported
Python's standard library `fnmatch` translates globs into regular expressions:
- `*`: Matches zero or more arbitrary characters, including spaces, slashes, punctuation, and newlines.
- `?`: Matches exactly one character.
- `[seq]`: Matches any character contained in `seq`.
- `[!seq]`: Matches any character not contained in `seq`.

### 4.3 Full Candidate Anchoring
`fnmatch` patterns are anchored to the entire string:
- Pattern `git push` matches only the exact string `"git push"`. It does not match `"git push origin main"`.
- Pattern `git push*` matches any string that begins with `"git push"`.
- Pattern `*git push*` matches any string containing `"git push"` anywhere.

---

## 5. Command Coverage, Compound Constructs, and Boundary Gaps

### 5.1 Verified Coverage
A rule anchored to the beginning of a command, such as `git push*`, successfully matches:
- Exact and extended arguments: `git push origin main`, `git push --force`
- Obfuscated command words: `git pu""sh origin main`, `git p\ush origin main`
- Line continuations: `git push \\\n --force`
- Case variations: `GIT PUSH ORIGIN MAIN`, `Git Push --force`
- Leading and trailing whitespace: `"  git push origin main  "`
- Nested shell payloads: `bash -c "git push origin main"`, `sh -lc "git push origin main"`

### 5.2 Critical Boundary Gap in Compound and Wrapped Commands
In Python, [`_command_detection_variants`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2412) does not split compound commands (`;`, `&&`, `||`), pipelines (`|`), subshells (`(...)`), brace groups (`{...}`), or command wrappers (`sudo`, `env`, variable assignments) into separate candidate variants.

Instead, [`_mark_command_starts`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L2294) inserts a newline `\n` into the single composite string. For example:
- Input: `echo starting && git push --force`
- Variants produced:
  1. `'echo starting && git push --force'`
  2. `'echo starting && \ngit push --force'`

Because `fnmatch.fnmatchcase` requires the pattern to match from index 0 of the candidate, a rule like `git push*` fails to match both variants because neither variant begins with `"git push"`!

| Command Construction | Leading Rule `git push*` | Wildcard Rule `*git push*` | Explanation |
| :--- | :--- | :--- | :--- |
| `git push --force` | **MATCH** | **MATCH** | Variant begins with `git push` |
| `bash -c "git push --force"` | **MATCH** | **MATCH** | Payload extracted by `_execution_flag_findings` |
| `echo start && git push --force` | **NO MATCH** | **MATCH** | Candidate begins with `echo start` |
| `echo start ; git push --force` | **NO MATCH** | **MATCH** | Candidate begins with `echo start` |
| `false || git push --force` | **NO MATCH** | **MATCH** | Candidate begins with `false` |
| `cat file \| git push --force` | **NO MATCH** | **MATCH** | Candidate begins with `cat file` |
| `(git push --force)` | **NO MATCH** | **MATCH** | Candidate begins with `(` |
| `{ git push --force; }` | **NO MATCH** | **MATCH** | Candidate begins with `{` |
| `GIT_TRACE=1 git push --force` | **NO MATCH** | **MATCH** | Candidate begins with `git_trace=1` |
| `sudo git push --force` | **NO MATCH** | **MATCH** | Candidate begins with `sudo` |
| `sudo -u deployer git push --force`| **NO MATCH** | **MATCH** | Candidate begins with `sudo -u` |

### 5.3 Safety Rule for Porting
The Python contract behavior is that users must configure `*pattern*` if they want to intercept commands embedded inside pipelines, subshells, compound commands, or behind wrappers like `sudo`. A native port must replicate this exact matching behavior to avoid breaking tests or diverging from established security expectations.

---

## 6. Precedence Hierarchy and Interaction with Bypass Modes

### 6.1 Evaluation Order in `check_all_command_guards`
In [`tools/approval.py:4744-4785`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L4744-L4785), command guards execute in strict sequence:

```
[1. Isolated Container Skip]
       |
       v (not isolated)
[2. Unconditional Hardline Floor] --------> BLOCK: hardline (root delete, fork bomb, etc.)
       |
       v (not hardline)
[3. Unconditional Sudo Stdin Guard] ------> BLOCK: sudo password guessing (sudo -S)
       |
       v (not sudo guess)
[4. User Deny Rules (approvals.deny)] ----> BLOCK: user deny rule (approvals.deny)
       |
       v (not denied)
[5. Bypass Gate: --yolo / /yolo / mode: off] -> APPROVE: bypass active ({"approved": True})
       |
       v (bypass not active)
[6. Permanent Allowlist] -----------------> APPROVE: allowlisted ({"approved": True})
       |
       v (not allowlisted)
[7. Dangerous Pattern Detection / Interactive Gate] -> PROMPT / FAIL CLOSED
```

### 6.2 Precedence Details
1. **Container Skip**: When `env_type == "docker"` without host bind mounts, or other isolated container backends, all guards are skipped and approval is granted immediately.
2. **Hardline Beats Deny**: If a command matches both the hardline blocklist (such as `rm -rf /`) and a user deny rule (such as `deny: ["*"]`), the hardline check fires first. The returned error identifies the hardline block (`"hardline": True`), not the user deny rule.
3. **Sudo Stdin Beats Deny**: If a command pipes passwords to `sudo -S` without `SUDO_PASSWORD` configured, it triggers the sudo-stdin guard block before user deny rules are evaluated.
4. **Deny Beats Bypass Modes**: User deny rules fire before `--yolo`, gateway session `/yolo`, and `approvals.mode: off`. A denied command is blocked even when bypass modes are active.
5. **Deny Beats Permanent Allowlist**: User deny rules fire before checking `command_allowlist`. A command listed in both is blocked.
6. **Mode Off Allows Non-Denied Commands**: Under `approvals.mode: off`, any command that is not hardline, not sudo-stdin guessing, and not matching a user deny rule is approved immediately (`approved: True, message: None`). No dangerous-pattern check or interactive prompt occurs.

---

## 7. Config Reload Timing, Live Mutation, and Last-Known-Good Invariant

### 7.1 Dynamic Reload Timing
Because [`_match_user_deny_rule`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L794) calls [`_get_approval_config()`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L3435), which invokes [`load_config_readonly()`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py#L3900), Python checks `config.yaml` file metadata on every command execution.
- If `config.yaml` is modified while the agent process is running, newly added or modified deny rules take effect on the very next command.

### 7.2 Last-Known-Good (LKG) Invariant
In [`hermes_cli/config.py:4161-4200`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py#L4161-L4200), if a live reload of `config.yaml` fails due to a YAML syntax error (for example, while a user is editing the file), Python does not fall back to empty defaults. Falling back to defaults would silently drop security-critical deny rules and expose the host.

Instead:
1. Python retains the in-memory last-known-good configuration ([`_LAST_EXPANDED_CONFIG_BY_PATH`](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/config.py#L4174)).
2. A backup of the corrupt file is written to disk (`config.yaml.corrupt.<timestamp>.bak`).
3. The previously loaded deny rules remain active until `config.yaml` is corrected.

---

## 8. Result Envelopes and Wire Contracts

### 8.1 Approval Module Envelopes

#### User Deny Block Result ([`_user_deny_block_result`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L826-L838))
```python
{
    "approved": False,
    "user_deny": True,
    "message": (
        f"BLOCKED: this command matches the user-defined deny rule "
        f"'{pattern}' (approvals.deny in config.yaml). It cannot be "
        "executed via the agent \u2014 not even with --yolo, /yolo, or "
        "approvals.mode=off. Do NOT retry or rephrase this command; "
        "the user has explicitly forbidden it."
    ),
}
```

Note: The Python string literal contains the Unicode em dash character `\u2014`.

#### Hardline Block Result ([`_hardline_block_result`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L856-L868))
```python
{
    "approved": False,
    "hardline": True,
    "message": (
        f"BLOCKED (hardline): {description}. This command is on the unconditional "
        "blocklist and cannot be executed via the agent \u2014 not even with --yolo, "
        "/yolo, approvals.mode=off, or cron approve mode. If you genuinely need "
        "to run it, run it yourself in a terminal outside the agent."
    ),
    "rule": description,
    "pattern_key": "hardline",
    "description": description,
}
```

#### Sudo Stdin Block Result ([`_sudo_stdin_block_result`](file:///home/eins0fx/development/hermes-agent-port/tools/approval.py#L740-L751))
```python
{
    "approved": False,
    "message": (
        f"BLOCKED: {description}. Do not pipe passwords to 'sudo -S' \u2014 "
        "this is a brute-force attack vector. Set SUDO_PASSWORD in your .env file "
        "if the agent needs passwordless sudo, or run the sudo command manually "
        "in your own terminal."
    ),
}
```

### 8.2 Terminal Tool Wire JSON Envelope ([`tools/terminal_tool.py:3331-3336`](file:///home/eins0fx/development/hermes-agent-port/tools/terminal_tool.py#L3331-L3336))
When a pre-execution guard rejects a command and `status != "pending_approval"`, `terminal_tool` formats the rejection into a JSON object:
```json
{
  "output": "",
  "exit_code": -1,
  "error": "BLOCKED: this command matches the user-defined deny rule 'git push*' (approvals.deny in config.yaml). It cannot be executed via the agent \u2014 not even with --yolo, /yolo, or approvals.mode=off. Do NOT retry or rephrase this command; the user has explicitly forbidden it.",
  "status": "blocked"
}
```

Key invariants:
- `output`: Always empty string `""`.
- `exit_code`: Always integer `-1`.
- `status`: Always string `"blocked"`.
- `error`: Contains the exact block message text.

---

## 9. Native Porting Assessment and Recommendations

### 9.1 Evaluation of Current Rust Gateway Seams
In [`rust/crates/hermes-gateway/src/main.rs:241-256`](file:///home/eins0fx/development/hermes-agent-port/rust/crates/hermes-gateway/src/main.rs#L241-L256):
```rust
fn native_local_terminal_eligible(config: &serde_json::Value) -> bool {
    let backend = config["terminal"]["backend"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("local");
    let approval_mode = config["approvals"]["mode"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("smart");
    let deny_is_empty = match config["approvals"].get("deny") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(rules)) => rules.is_empty(),
        Some(_) => false,
    };
    cfg!(unix) && backend == "local" && approval_mode == "off" && deny_is_empty
}
```

Currently, `deny_is_empty` is enforced because user deny rule matching was deferred during the initial foreground terminal implementation. If a profile has non-empty deny rules, the gateway refrains from advertising the native terminal and delegates execution to the Python extension host.

### 9.2 Safe Eligibility Expansion
A static deny-rule slice in Rust can safely remove the `&& deny_is_empty` condition so that:
```rust
cfg!(unix) && backend == "local" && approval_mode == "off"
```
is eligible for native execution.

### 9.3 Requirements for the Native Static Deny-Rule Slice
To maintain 100% security parity with Python, the Rust slice must implement:
1. **Rule Ingestion**: Parse `approvals.deny` from configuration, trimming whitespace and filtering non-strings.
2. **Detection Variants**: Generate command variants matching Section 3 (ANSI stripping, empty quote removal, backslash collapse, IFS normalization, Windows slash flattening, and shell `-c` payload extraction).
3. **fnmatch Matching**: Implement case-insensitive glob matching matching Section 4.
4. **Guard Precedence**: Maintain strict evaluation order: hardline check -> sudo stdin guard -> user deny rules -> mode: off bypass.
5. **JSON Result Envelope**: Match the exact JSON wire format (`exit_code: -1`, `status: "blocked"`, `output: ""`, and verbatim error text).
6. **Config LKG & Reload**: Ensure that mid-turn edits to `config.yaml` are recognized or safely bounded, and corrupted config files do not drop deny protections.

### 9.4 Hard Blockers for Non-Off Modes
This finding applies strictly to `approvals.mode: off`. If `approvals.mode` is `"manual"`, `"smart"`, or `"ask"`, non-denied dangerous commands require interactive human approval. As established in the gateway architecture audit ([`rust/analysis/native-approval-wiring-claude.md`](file:///home/eins0fx/development/hermes-agent-port/rust/analysis/native-approval-wiring-claude.md)), interactive approval in Rust is currently blocked by the per-session turn lease architecture and the absence of bidirectional mid-turn tool communication. Those modes must remain routed through the Python agent until interactive suspension is resolved.

---

## 10. Oracle and Golden Verification

The contract defined in this document is backed by two deterministic artifacts:
- **Oracle Script**: [`rust/tools/approval-deny-contract-oracle.py`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/approval-deny-contract-oracle.py)
- **Golden Test Corpus**: [`rust/tools/approval-deny-contract-goldens.json`](file:///home/eins0fx/development/hermes-agent-port/rust/tools/approval-deny-contract-goldens.json)

### 10.1 Corpus Summary (59 Cases)
- **Rule Parsing (8 cases)**: Empty list, None, empty string, whitespace string, whitespace trimming, mixed non-string filtering, rule ordering, case preservation.
- **Normalization & Variants (13 cases)**: Upper command, mixed rule and command, empty double quotes, empty single quotes, backslash escape, word deobfuscation, line continuation, IFS expansion, Windows path flattening, bash `-c` payload, sh `-lc` payload, nested bash, grep masking.
- **fnmatch Glob Semantics (12 cases)**: Star prefix, star suffix, star infix, star slashes/flags, question mark single char, question mark rejection, bracket character class, bracket negation class, exact match without wildcard.
- **Command Coverage & Boundaries (10 cases)**: Bare command, compound AND (`&&`), compound semicolon (`;`), compound OR (`||`), pipeline (`|`), subshell parens (`(...)`), brace group (`{...}`), env var prefix, sudo wrapper, sudo with options.
- **Precedence & Bypass (9 cases)**: Container skip, container host access, hardline beats deny, sudo stdin beats deny, deny beats yolo, deny beats mode: off, deny beats allowlist, mode: off allows non-denied dangerous command, mode: off allows safe command.
- **Terminal Tool Wire Envelopes (4 cases)**: User deny block envelope, hardline block envelope, sudo stdin block envelope, mode: off allowed execution envelope.
- **Config Reload & LKG (3 cases)**: Initial valid load, mtime update reload, corrupt YAML last-known-good retention.

### 10.2 Verification Command
The goldens can be verified at any time with:
```bash
.venv/bin/python rust/tools/approval-deny-contract-oracle.py --check
```
Both the oracle and goldens run hermetically, require no network or credentials, and execute zero candidate shell commands.
