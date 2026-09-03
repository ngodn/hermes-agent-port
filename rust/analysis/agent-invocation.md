# Agent Invocation Analysis: Headless Single-Turn Execution for Rust-Python Interop

**Target Document**: `rust/analysis/agent-invocation.md`  
**Date**: September 2026  
**Context**: Planning Python-to-Rust port / Strangler Fig migration (Phase 2 & Phase 4). Evaluating how a Rust process (e.g. `crates/hermes-gateway` or a CLI host) can spawn the Python agent as a subprocess, submit a single user turn, and stream tokens/events back.

---

## Executive Summary & Direct Answer

| Question | Status | Details |
|---|---|---|
| **Clean single-turn headless streaming CLI?** | **Does NOT exist** | Current single-turn CLI modes (`hermes -z`, `hermes chat -q -Q`) are strictly **blocking and non-streaming** (they buffer the whole turn, silence all intermediate callbacks, and print only the final text upon completion). |
| **Clean single-turn non-streaming CLI?** | **Exists** | `hermes -z "<prompt>"` / `hermes --oneshot "<prompt>"` (`hermes_cli/oneshot.py`). Prints only final response to stdout; exits 0/1/2. |
| **Interactive streaming CLI?** | **Exists (Human TTY)** | `hermes chat -q "<prompt>"` streams to a Rich terminal console (ANSI escape codes, spinners, Markdown boxes), not structured machine data. |
| **Headless streaming over stdio?** | **Exists (Multi-Turn RPC Daemons)** | `tui_gateway` (JSON-RPC over stdio lines) and `acp_adapter` (Anthropic Agent Client Protocol over stdio). Both are long-lived daemons, not single-turn exit-on-complete CLI runs. |
| **Headless streaming over HTTP/SSE?** | **Exists (Daemon)** | `gateway/platforms/api_server.py` exposes OpenAI-compatible `/v1/chat/completions` (with `stream: true`) and `/v1/runs/{id}/events` SSE streams. In-process inside gateway. |
| **Gateway subprocess spawning?** | **Not used for agent** | `gateway/run.py` does **not** spawn `run_agent.py` as a subprocess; it imports `from run_agent import AIAgent` directly in-process and runs on worker threads. |

To allow a Rust process to spawn a Python subprocess for a **single turn with token/event streaming**, a thin CLI wrapper or flag (e.g. `hermes -z "<prompt>" --stream-jsonl` or `python -m hermes_cli.stream_turn`) must be added to hook `AIAgent`'s streaming callbacks (`stream_delta_callback`, `tool_start_callback`, `tool_complete_callback`) and emit newline-delimited JSON (JSON Lines) to stdout.

---

## 1. Concrete Commands & Flag Definitions

### Entrypoint 1: Top-Level Oneshot Mode (`hermes -z` / `hermes --oneshot`)

This is the primary non-interactive entrypoint designed for scripts and pipelines.

* **Argparse Flag Definitions** ([hermes_cli/_parser.py:154-176](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/_parser.py#L154-L176)):
  ```python
  parser.add_argument(
      "-z",
      "--oneshot",
      metavar="PROMPT",
      default=None,
      help=(
          "One-shot mode: send a single prompt and print ONLY the final "
          "response text to stdout. No banner, no spinner, no tool "
          "previews, no session_id line. Tools, memory, rules, and "
          "AGENTS.md in the CWD are loaded as normal; approvals are "
          "auto-bypassed. Intended for scripts / pipes."
      ),
  )
  parser.add_argument(
      "--usage-file",
      metavar="PATH",
      default=None,
      help=(
          "One-shot mode only: after the run, write a JSON usage report "
          "(estimated cost, token counts, model, api_calls) to PATH. "
          "The report is written even when the run fails, so pipelines "
          "can always account for spend. No effect outside -z/--oneshot."
      ),
  )
  ```

* **Dispatch Sites** ([hermes_cli/main.py:13095-13104](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/main.py#L13095-L13104), [13152-13162](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/main.py#L13152-L13162), [15152-15162](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/main.py#L15152-L15162)):
  ```python
  if getattr(args, "oneshot", None):
      _confirm_startup_expensive_model_override(args)
      _run_and_exit_oneshot(
          args.oneshot,
          model=getattr(args, "model", None),
          provider=getattr(args, "provider", None),
          toolsets=getattr(args, "toolsets", None),
          skills=getattr(args, "skills", None),
          usage_file=getattr(args, "usage_file", None),
      )
  ```

* **Implementation** ([hermes_cli/oneshot.py:202-338](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/oneshot.py#L202-L338)):
  `run_oneshot(prompt, model=None, provider=None, toolsets=None, skills=None, usage_file=None)` builds `AIAgent` directly, sets `quiet_mode=True`, calls `agent.run_conversation(prompt)`, writes the final response string to stdout, optionally writes `--usage-file`, and exits past interpreter finalizers using `os._exit()` ([hermes_cli/main.py:134-160](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/main.py#L134-L160)).

---

### Entrypoint 2: Chat Subcommand Single-Query Mode (`hermes chat -q ...`)

* **Argparse Flag Definitions** ([hermes_cli/_parser.py:353-385, 448-452](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/_parser.py#L353-L385)):
  ```python
  _query_group = chat_parser.add_mutually_exclusive_group()
  _query_group.add_argument(
      "-q", "--query",
      help=(
          "Query to run. On a real TTY the prompt seeds an interactive "
          "session (submitted literally as the first turn); combined with "
          "--oneshot or -Q, or on a non-TTY, it answers and exits."
      ),
  )
  _query_group.add_argument(
      "--query-file",
      metavar="PATH",
      help=(
          "Read the single query from a file instead of the command line "
          "('-' reads stdin). Safe for arbitrary text: nothing is shell-"
          "interpreted, so quotes, $(...), and backticks are preserved "
          "verbatim. Mutually exclusive with -q."
      ),
  )
  chat_parser.add_argument(
      "--oneshot",
      dest="oneshot_exit",
      action="store_true",
      default=False,
      help=(
          "With -q/--query-file: answer the query and exit (legacy "
          "single-query behavior) instead of seeding an interactive "
          "session. Implied on non-TTY stdio and by -Q/--quiet."
      ),
  )
  chat_parser.add_argument(
      "-Q",
      "--quiet",
      action="store_true",
      help="Quiet mode for programmatic use: suppress banner, spinner, and tool previews. Only output the final response and session info.",
  )
  ```

* **Fire CLI Definition in `cli.py`** ([cli.py:21770-21797, 22423-22426](file:///home/eins0fx/development/hermes-agent-port/cli.py#L21770-L21797)):
  ```python
  def main(
      query: str = None,
      q: str = None,
      oneshot: bool = False,
      image: str = None,
      toolsets: str = None,
      skills: str | list[str] | tuple[str, ...] = None,
      model: str = None,
      provider: str = None,
      reasoning: str = None,
      api_key: str = None,
      base_url: str = None,
      max_turns: int = None,
      run_budget: float = None,
      verbose: Optional[bool] = None,
      quiet: bool = False,
      compact: bool = False,
      list_tools: bool = False,
      list_toolsets: bool = False,
      gateway: bool = False,
      resume: str = None,
      worktree: bool = False,
      w: bool = False,
      checkpoints: bool = False,
      pass_session_id: bool = False,
      ignore_user_config: bool = False,
      ignore_rules: bool = False,
  ):
      ...
  if __name__ == "__main__":
      import fire
      fire.Fire(main)
  ```

---

### Entrypoint 3: Direct Agent Invocation (`python run_agent.py`)

* **Fire CLI Definition** ([run_agent.py:9959-9972, 10173-10175](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L9959-L9972)):
  ```python
  def main(
      query: str = None,
      model: str = "",
      api_key: str = None,
      base_url: str = "",
      max_turns: int = 10,
      enabled_toolsets: str = None,
      disabled_toolsets: str = None,
      list_tools: bool = False,
      save_trajectories: bool = False,
      save_sample: bool = False,
      verbose: bool = False,
      log_prefix_chars: int = 20
  ):
      ...
  if __name__ == "__main__":
      import fire
      fire.Fire(main)
  ```
  *Behavior*: Intended for manual development/testing. Emits verbose header/footer banners (`🤖 AI Agent with Tool Calling`, `📋 CONVERSATION SUMMARY`). Not machine-readable.

---

### Entrypoint 4: Batch & Benchmark Runners (`batch_runner.py`, `mini_swe_runner.py`)

* **`batch_runner.py`** ([batch_runner.py:1206-1230](file:///home/eins0fx/development/hermes-agent-port/batch_runner.py#L1206-L1230)):
  Parallel dataset processing via multiprocessing `Pool`. CLI takes `--dataset_file=data.jsonl`, `--batch_size=10`, `--num_workers=4`, `--distribution=default`. Outputs trajectory files (`trajectories.jsonl`, `stats.json`). Not for single-turn interactive execution.
* **`mini_swe_runner.py`** ([mini_swe_runner.py:630-643](file:///home/eins0fx/development/hermes-agent-port/mini_swe_runner.py#L630-L643)):
  Lightweight SWE runner using Terminal tool only with Docker/Modal/Local sandboxes. CLI takes `--task="prompt"` or `--prompts_file=prompts.jsonl`, writing output to JSONL.

---

### Entrypoint 5: Gateway Subprocess Analysis (`gateway/run.py`)

Search for `asyncio.create_subprocess_*` in `gateway/`:
1. [gateway/run.py:3769](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L3769): Spawns `ffprobe` to probe audio media duration.
2. [gateway/run.py:20112](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py#L20112): Spawns `/exec` quick commands via `asyncio.create_subprocess_shell`.
3. [gateway/platforms/whatsapp_cloud.py:1282](file:///home/eins0fx/development/hermes-agent-port/gateway/platforms/whatsapp_cloud.py#L1282) & [gateway/platforms/qqbot/adapter.py:2152](file:///home/eins0fx/development/hermes-agent-port/gateway/platforms/qqbot/adapter.py#L2152): Media transcoding with `ffmpeg`.
4. **Agent Execution**: `gateway/run.py` does **NOT** spawn subprocesses for agent execution. It executes `AIAgent` directly in-process (`from run_agent import AIAgent` at lines `1952`, `21927`, `25499`, `31358`).

---

### Entrypoint 6: Stdio RPC Server Daemons (`tui_gateway`, `acp_adapter`)

These are persistent line-based JSON-RPC daemons over stdio:
* **`tui_gateway/entry.py:422-517`** ([tui_gateway/entry.py:486-514](file:///home/eins0fx/development/hermes-agent-port/tui_gateway/entry.py#L486-L514)):
  Reads line-delimited JSON-RPC from `sys.stdin`, dispatches to handlers (`prompt.submit` at [tui_gateway/methods_prompt.py:287](file:///home/eins0fx/development/hermes-agent-port/tui_gateway/methods_prompt.py#L287)), and writes JSON-RPC events/responses to `sys.stdout`.
* **`acp_adapter/entry.py:1-14`** ([acp_adapter/server.py](file:///home/eins0fx/development/hermes-agent-port/acp_adapter/server.py)):
  Implements the standard Anthropic Agent Client Protocol (ACP) over stdio for IDE integration.

---

## 2. Required Arguments & Environment Variables

To run a single agent turn headlessly, the following configuration parameters and environment variables are evaluated:

| Setting | CLI Flag (`hermes`) | CLI Flag (`cli.py`) | Environment Variable | Fallback / Default |
|---|---|---|---|---|
| **Model** | `-m`, `--model` | `--model` | `HERMES_INFERENCE_MODEL` | `config.yaml` (`model.default` / `model.model`), or `anthropic/claude-sonnet-4.6` |
| **Provider** | `--provider` | `--provider` | `HERMES_INFERENCE_PROVIDER` | Auto-detected from model name, or `config.yaml` (`model.provider`), or `"auto"` |
| **API Key** | N/A | `--api_key` | `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc. | Loaded from `~/.hermes/.env` or `.env` |
| **Hermes Home** | `-p`, `--profile <name>` | N/A | `HERMES_HOME` | `~/.hermes` (or `~/.hermes/profiles/<name>`) |
| **Toolsets** | `-t`, `--toolsets` | `--toolsets` | N/A | `config.yaml` (`tools.cli` enabled toolsets) |
| **Skills** | `-s`, `--skills` | `--skills` | N/A | Built-in discovery in `skills/` & `~/.hermes/skills/` |
| **Bypass Approvals** | `--yolo` | N/A | `HERMES_YOLO_MODE=1` | **Required for headless runs** (auto-set by `oneshot.py:255`) |
| **Bypass Shell Hooks** | `--accept-hooks` | N/A | `HERMES_ACCEPT_HOOKS=1` | **Required for headless runs** (auto-set by `oneshot.py:256`) |
| **Single-Query Gate** | N/A | N/A | `HERMES_SINGLE_QUERY_SESSION=1` | Prevents approval timeouts when non-interactive (`cli.py:22179`) |
| **Non-Interactive Flag** | N/A | N/A | Unset `HERMES_INTERACTIVE` | Must NOT be set to `"1"` in headless runs (avoids blocking sudo password prompts) |
| **Stateless Delegation** | N/A | N/A | `declare_stateless_channel()` | Forces `delegate_task` to run subagents synchronously inline instead of backgrounding (`oneshot.py:265`) |
| **Usage Report** | `--usage-file <path>` | N/A | N/A | Writes JSON accounting metrics on exit |

---

## 3. Output & Streaming Emission Analysis

### Why Existing Single-Turn Modes Do Not Stream

In `hermes -z` ([hermes_cli/oneshot.py:227-328](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/oneshot.py#L227-L328)):
1. All logging and standard streams are suppressed to `/dev/null` during the entire turn:
   ```python
   # hermes_cli/oneshot.py:227-231, 277
   logging.disable(logging.CRITICAL)
   with redirect_stdout(devnull), redirect_stderr(devnull):
       response, result = _run_agent(...)
   ```
2. Streaming callbacks on `AIAgent` are explicitly cleared:
   ```python
   # hermes_cli/oneshot.py:522-524
   agent.suppress_status_output = True
   agent.stream_delta_callback = None
   agent.tool_gen_callback = None
   ```
3. Only the complete final text is written to `real_stdout` upon completion:
   ```python
   # hermes_cli/oneshot.py:325-328
   if response:
       real_stdout.write(response)
       if not response.endswith("
"):
           real_stdout.write("
")
       real_stdout.flush()
   ```

Similarly, in `cli.py` quiet single-query mode ([cli.py:22300-22362](file:///home/eins0fx/development/hermes-agent-port/cli.py#L22300-L22362)):
```python
# cli.py:22300-22314
cli.agent.reasoning_callback = None
cli.agent.tool_progress_callback = None
cli.agent.tool_start_callback = None
cli.agent.tool_complete_callback = None
cli.agent.tool_progress_mode = "off"
result = cli.agent.run_conversation(user_message=effective_query, ...)
...
# cli.py:22346, 22362
if response:
    print(response) # Final text to stdout
print(f"
session_id: {cli.session_id}", file=sys.stderr) # Session ID to stderr
```

### How Streaming Works Internally on `AIAgent`

When streaming is enabled (e.g. in `gateway/platforms/api_server.py` or `tui_gateway/server.py`), `AIAgent` ([run_agent.py:490-560](file:///home/eins0fx/development/hermes-agent-port/run_agent.py#L490-L560)) exposes these synchronous callback hooks:

* `stream_delta_callback(text: str)`: Emits assistant text deltas as tokens arrive from the provider.
* `thinking_callback(delta: str)` / `reasoning_callback(...)`: Emits reasoning / thinking block tokens.
* `tool_start_callback(tool_name: str, tool_args: dict)`: Fired before a tool executes.
* `tool_progress_callback(event_type: str, data: dict)`: Fired during tool progress.
* `tool_complete_callback(tool_name: str, result: str, ...)`: Fired after tool execution finishes.

---

## 4. Input Passing Mechanisms

| Input Channel | CLI Form | Internal Handling Code |
|---|---|---|
| **CLI Argument (argv)** | `hermes -z "PROMPT"` or `hermes chat -q "PROMPT"` | `args.oneshot` ([hermes_cli/main.py:13095](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/main.py#L13095)), `args.query` ([cli.py:21770](file:///home/eins0fx/development/hermes-agent-port/cli.py#L21770)) |
| **File Path** | `hermes chat --query-file /path/to/prompt.txt` | Reads file verbatim without shell interpretation ([hermes_cli/_parser.py:361](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/_parser.py#L361)) |
| **Standard Input (stdin)** | `hermes chat --query-file -` | Reads `sys.stdin.read()` verbatim |
| **Image Attachments** | `hermes chat -q "..." --image /path/to/img.png` | `_collect_query_images` in [cli.py:22160, 22183](file:///home/eins0fx/development/hermes-agent-port/cli.py#L22160); routes as multimodal content part or OCR |
| **JSON-RPC Stdin (Daemon)** | Line-delimited JSON on stdin | `sys.stdin.readline()` in [tui_gateway/entry.py:487](file:///home/eins0fx/development/hermes-agent-port/tui_gateway/entry.py#L487) calling `prompt.submit` |

---

## 5. Statefulness & Disk/DB Dependencies

1. **Does a single turn require pre-existing sessions/DB?**
   * **No.** It does **not** require any existing session or pre-populated database rows. A turn can execute with a completely fresh session identifier.
2. **Does it touch the disk / SQLite DB by default?**
   * **Yes, best-effort SessionDB SQLite connection**:
     [hermes_cli/oneshot.py:341-355](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/oneshot.py#L341-L355) opens `~/.hermes/state.db` so the agent's internal `session_search` tool functions. If the database file is unavailable or corrupted, it catches the error and gracefully sets `session_db = None`.
   * **Customizations / Context files**:
     By default, `AIAgent` loads `AGENTS.md`, `SOUL.md`, `.cursorrules`, and memory files from the working directory / `~/.hermes/` unless `--ignore-rules` / `--ignore-user-config` / `--safe-mode` is supplied ([hermes_cli/_parser.py:301-318](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/_parser.py#L301-L318)).
3. **Task Delegation Behavior**:
   * Calling `declare_stateless_channel()` ([hermes_cli/oneshot.py:265](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/oneshot.py#L265)) signals that background subagent completions cannot re-enter later turns, ensuring subagent tasks run inline/synchronously.

---

## 6. Gap Analysis & Recommended Rust Invocation Strategy

### The Problem

If the Rust process spawns:
1. `hermes -z "prompt"`: Rust gets **no streaming** — it blocks until the entire turn finishes, then reads the full output at once.
2. `hermes chat -q "prompt"`: Rust gets an **interactive ANSI TTY stream** with terminal control sequences, spinner escapes, and boxed Markdown diffs.
3. `tui_gateway`: Rust must implement the full **JSON-RPC bidirectional handshake protocol** (`gateway.ready`, `prompt.submit`, keepalives).

### Recommended Solution: Dedicated Headless Streaming Entrypoint

For Phase 2/4 of the port, create a lightweight Python CLI entrypoint: `python -m hermes_cli.stream_turn` (or flag `hermes -z "prompt" --stream jsonl`).

#### Python Side (`hermes_cli/stream_turn.py`)
```python
"""Single-turn JSONL streaming entrypoint for subprocess callers (e.g. Rust)."""
import json
import os
import sys
from gateway.session_context import declare_stateless_channel
from hermes_cli.config import load_config
from hermes_cli.runtime_provider import resolve_runtime_provider
from run_agent import AIAgent

def emit(event_type: str, data: dict):
    payload = {"event": event_type, "data": data}
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "
")
    sys.stdout.flush()

def main():
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("-p", "--prompt", default="-", help="Prompt text or '-' for stdin")
    parser.add_argument("-m", "--model", default=None)
    parser.add_argument("--provider", default=None)
    parser.add_argument("-t", "--toolsets", default=None)
    parser.add_argument("--session-id", default=None)
    args = parser.parse_args()

    prompt = sys.stdin.read() if args.prompt == "-" else args.prompt
    if not prompt.strip():
        sys.exit(0)

    # Configure headless execution guards
    os.environ["HERMES_YOLO_MODE"] = "1"
    os.environ["HERMES_ACCEPT_HOOKS"] = "1"
    declare_stateless_channel()

    cfg = load_config()
    runtime = resolve_runtime_provider(requested=args.provider, target_model=args.model)

    agent = AIAgent(
        api_key=runtime.get("api_key"),
        base_url=runtime.get("base_url"),
        provider=runtime.get("provider"),
        model=args.model or runtime.get("model"),
        quiet_mode=True,
        stream_delta_callback=lambda text: emit("text_delta", {"text": text}),
        thinking_callback=lambda text: emit("thinking_delta", {"text": text}),
        tool_start_callback=lambda name, args: emit("tool_start", {"tool": name, "args": args}),
        tool_complete_callback=lambda name, result, **kw: emit("tool_complete", {"tool": name, "result": str(result)[:500]}),
    )

    try:
        result = agent.run_conversation(prompt)
        emit("done", {
            "completed": result.get("completed", True),
            "final_response": result.get("final_response", ""),
            "input_tokens": result.get("input_tokens", 0),
            "output_tokens": result.get("output_tokens", 0),
        })
    finally:
        agent.close()

if __name__ == "__main__":
    main()
```

#### Rust Subprocess Spawning Example (`tokio::process`)
```rust
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use std::process::Stdio;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum AgentStreamEvent {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { text: String },
    #[serde(rename = "tool_start")]
    ToolStart { tool: String, args: serde_json::Value },
    #[serde(rename = "tool_complete")]
    ToolComplete { tool: String, result: String },
    #[serde(rename = "done")]
    Done { completed: bool, final_response: String, input_tokens: u64, output_tokens: u64 },
}

pub async fn run_agent_turn(prompt: &str, model: Option<&str>) -> anyhow::Result<()> {
    let mut cmd = Command::new("python3");
    cmd.args(["-m", "hermes_cli.stream_turn", "--prompt", "-"])
       .stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::inherit());

    if let Some(m) = model {
        cmd.args(["--model", m]);
    }

    let mut child = cmd.spawn()?;
    
    // Write prompt to stdin and close stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.flush().await?;
    }

    let stdout = child.stdout.take().expect("stdout captured");
    let mut reader = BufReader::new(stdout).lines();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() { continue; }
        let event: AgentStreamEvent = serde_json::from_str(&line)?;
        match event {
            AgentStreamEvent::TextDelta { text } => print!("{text}"),
            AgentStreamEvent::ToolStart { tool, .. } => println!("
[Tool: {tool}]"),
            AgentStreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("Agent process exited with code {:?}", status.code());
    }
    Ok(())
}
```

---

## 7. Summary Table of Files Investigated

| File | Primary Role | Headless Invocation Findings |
|---|---|---|
| [hermes_cli/_parser.py](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/_parser.py) | CLI Argparse definitions | Defines `-z/--oneshot`, `--usage-file`, `-m/--model`, `--provider`, `-q/--query`, `--query-file`, `-Q/--quiet`. |
| [hermes_cli/oneshot.py](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/oneshot.py) | Implementation of `-z` | Non-streaming blocking turn. Mutes stdio to `/dev/null`, disables streaming callbacks, sets `HERMES_YOLO_MODE=1` & `HERMES_ACCEPT_HOOKS=1`, calls `declare_stateless_channel()`, writes final response to stdout. |
| [hermes_cli/main.py](file:///home/eins0fx/development/hermes-agent-port/hermes_cli/main.py) | CLI launcher & fast-path | Dispatches `-z` via `_run_and_exit_oneshot`. Bypasses interpreter teardown with `os._exit()`. |
| [cli.py](file:///home/eins0fx/development/hermes-agent-port/cli.py) | Interactive REPL & Fire CLI | Handles `-q` single-query mode. In `-Q` (quiet), disables progress callbacks and prints final response to stdout and `session_id` to stderr upon completion. |
| [run_agent.py](file:///home/eins0fx/development/hermes-agent-port/run_agent.py) | Agent Core Loop (`AIAgent`) | `main()` uses `fire.Fire(main)` with decorative banners. Exposes `stream_delta_callback`, `tool_start_callback`, etc. on `AIAgent`. |
| [batch_runner.py](file:///home/eins0fx/development/hermes-agent-port/batch_runner.py) | Batch Dataset Processor | Uses multiprocessing pool to run prompts over dataset JSONL files, saving trajectories. Not a single-turn agent CLI. |
| [mini_swe_runner.py](file:///home/eins0fx/development/hermes-agent-port/mini_swe_runner.py) | Lightweight SWE runner | Terminal-only sandbox execution runner writing trajectory JSONL. |
| [gateway/run.py](file:///home/eins0fx/development/hermes-agent-port/gateway/run.py) | Multi-platform Gateway Daemon | Does **not** spawn agent subprocesses; imports `AIAgent` directly in-process. Spawns `ffprobe` and shell `/exec` quick commands only. |
| [gateway/platforms/api_server.py](file:///home/eins0fx/development/hermes-agent-port/gateway/platforms/api_server.py) | HTTP REST / SSE Server | Implements OpenAI-compatible `/v1/chat/completions` SSE streaming over HTTP using `stream_delta_callback`. |
| [tui_gateway/entry.py](file:///home/eins0fx/development/hermes-agent-port/tui_gateway/entry.py) | TUI Stdio RPC Gateway | Stdio JSON-RPC server daemon reading requests from stdin and streaming events to stdout. |
| [acp_adapter/entry.py](file:///home/eins0fx/development/hermes-agent-port/acp_adapter/entry.py) | Agent Client Protocol (ACP) | Stdio JSON-RPC ACP server daemon for editor/IDE integration. |
