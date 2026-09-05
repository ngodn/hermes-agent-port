# Porting helpers

Codex coordinates the rewrite and owns review, integration, and Cargo builds.
Helpers receive bounded tasks with explicit output-file ownership. They do not
commit or run competing Cargo builds. Keep prompts under `tasks/` and results
under `../analysis/`.

Follow the user's Rust layout preference: tests belong in the implementation
file, inside `#[cfg(test)] mod tests` or an inline `golden_corpus` module like
`config_loader.rs`. Do not create sibling `*_parity.rs` or `*_test.rs` files.
Keep comments focused on the Python source contract, non-obvious behavior,
ownership, and integration boundaries. Fixture data and generators stay under
`rust/tools/`; that data is shared evidence, not another Rust test module.

```bash
rust/tools/claude.sh rust/tools/tasks/port-inbound-media.txt > rust/analysis/port-inbound-media.claude.json 2> rust/analysis/port-inbound-media.claude.stderr
rust/tools/agy.sh rust/tools/tasks/audit-tier2.txt > rust/analysis/tier2-source-audit.agy.log 2>&1
```

Claude is pinned to `claude-opus-4-8`, `--effort medium`. Gemini is pinned to
`gemini-3.8-flash-high`. Both use `--dangerously-skip-permissions`, as explicitly
requested by the user. There is no fallback to another model. Claude has a
15-minute process timeout and emits a JSON result with error and model-usage
fields. Check the process exit status and `is_error` before accepting output.
An agent report is evidence to inspect, not proof that the port is correct.

Claude resolves the mise-managed executable directly when available. On this
machine `~/.local/bin/claude` runs `mise use -g` before launching; using the
managed executable avoids changing global settings and keeps stdout as JSON.
The initial port log predates that fix and has one mise status line before its
JSON envelope. The follow-up review uses the corrected wrapper.

AGY retains its existing exclusive flock. Let queued AGY work serialize rather
than launching the raw CLI concurrently. This preserves the workaround for
the refresh race recorded in this project's history. Its lock is local to
these Hermes helper invocations, not shared with the separate dgnrt engine.
The wrapper also prefixes prompts with the absolute checkout path because its
filesystem tools have sometimes started outside the CLI working directory.

## Reference app

Source: `/home/eins0fx/development/eins0fx.xn--6frz82g/dgnrt`.
The current engine uses `engine/src/ai/agy.rs`: a semaphore limits jobs and an
AuthGate serializes cold authentication, opening a 15-minute warm window after
success. The Rust port wrapper conservatively serializes the whole invocation.

Commit `0f48fa4` migrated the older Claude brain to AGY. Its parent retains
`ops/brain/brain.mjs`, which used the Claude Agent SDK with explicit model,
bounded turns, result parsing, and permission bypass. It restricted tools for
scraped-content tasks. We use the installed CLI for development helper tasks;
no SDK dependency or new Hermes runtime model tool is needed.

CLI references consulted:
[Claude CLI](https://code.claude.com/docs/en/cli-reference) and
[AGY headless mode](https://antigravity.google/docs/cli/headless/).
Installed CLI help and successful invocation results determine the local flags
and model support.

## Differential media fixtures

The project requires Python 3.11-3.13. The system PATH currently selects 3.14;
use the installed mise Python 3.12.13 for the standalone fixture generator:

```bash
mise exec python@3.12.13 -- python rust/tools/gen_inbound_media_goldens.py
mise exec python@3.12.13 -- python rust/tools/gen_inbound_media_goldens.py --check
cd rust
cargo test -p hermes-gateway inbound_media
```

The generator executes extracted source bodies without importing the gateway
or starting external services. Rust tests compare real return values against
those fixtures. Full Python imports and real turn-path testing still belong to
the later integration milestone.

Additional source-executed fixture generators now cover context notes,
pending-STT transitions (including the combined transcribe/echo flow), and
pending-event merging. Run each with the same mise Python and `--check`:
`gen_media_context_goldens.py`, `gen_pending_stt_goldens.py`, and
`gen_pending_message_goldens.py`. See
[the verification record](../analysis/inbound-state-verification.md) for scope
and the differences caught during integration.
