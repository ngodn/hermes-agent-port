//! Provider-agnostic tool-calling loop for the native agent.
//!
// Ahead of its wiring into run_turn; exercised by tests. Allow dead code for now.
#![allow(dead_code)]
//!
//! The turn loop that turns a single user message into a final answer, running
//! any tool calls the model requests along the way. It is generic over a
//! [`ChatModel`] so it can be driven by the OpenAI-compatible HTTP client
//! ([`crate::native_agent::NativeAgentClient`]) or, later, a CLI backend
//! (Claude Code / Antigravity), and unit-tested with a stub model.
//!
//! OpenAI tool-calling shape: tools are sent as
//! `{"type":"function","function":{name,description,parameters}}`; the model
//! replies with `message.tool_calls[]` (each `{id, function:{name, arguments}}`
//! where `arguments` is a JSON *string*); tool results are appended as
//! `{"role":"tool","tool_call_id":id,"content":...}` and the loop repeats until
//! the model returns plain content.

use std::sync::Arc;

use async_trait::async_trait;
use hermes_core::{Error, Result, StreamEvent};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::warn;

/// Metadata describing a tool to the model.
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: Value,
}

/// A callable tool. `call` runs synchronously; long-running tools can block on
/// their own runtime handle if needed.
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn call(&self, args: &Value) -> Result<String>;
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed arguments (the wire form is a JSON string, decoded here).
    pub arguments: Value,
}

/// One model step: either it wants tools run, or it produced a final answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    ToolCalls {
        calls: Vec<ToolCall>,
        /// Replay data is distinct from decoded execution arguments. Providers
        /// can require signatures and the original argument string verbatim.
        assistant_message: Value,
    },
    Final(String),
}

/// A chat model that can take a message list + tool specs and return the next
/// step. Implemented by the HTTP client and (later) CLI backends.
#[async_trait]
pub trait ChatModel: Send + Sync {
    async fn step(&self, messages: &[Value], tools: &[Value]) -> Result<Step>;
}

/// Convert a [`ToolSpec`] to the OpenAI `tools` array entry.
pub fn tool_spec_json(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
        }
    })
}

/// Build the assistant message that echoes the model's tool calls, as required
/// before appending their results.
#[cfg(test)]
fn assistant_tool_calls_msg(calls: &[ToolCall]) -> Value {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.arguments.to_string() }
            })
        })
        .collect();
    json!({ "role": "assistant", "content": null, "tool_calls": tool_calls })
}

/// Parse the `message` object of a non-streaming completion into a [`Step`].
pub fn parse_message_step(message: &Value) -> Step {
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        if !calls.is_empty() {
            let mut parsed = Vec::new();
            let mut replay_calls = Vec::new();
            for call in calls {
                let Some(id) = call.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // Composite bridge IDs pair a call with a provider item. Both
                // assistant replay and its result must use the canonical half.
                let id = id
                    .split_once('|')
                    .map(|(call_id, _)| {
                        call_id.trim_matches(crate::python_value::python_whitespace)
                    })
                    .unwrap_or(id);
                let Some(function) = call.get("function") else {
                    continue;
                };
                let Some(name) = function.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let decoded = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| {
                        serde_json::from_str::<Value>(raw)
                            .ok()
                            .map(|value| (raw, value))
                    });
                // Preserve valid text exactly. Until the Python malformed-JSON
                // repair pipeline is ported, retain the existing empty-object
                // fallback for inputs that cannot be decoded for execution.
                let (raw_arguments, arguments) = match decoded {
                    Some((raw, arguments)) => (json!(raw), arguments),
                    None => (json!("{}"), json!({})),
                };
                parsed.push(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                });
                // Keep valid wire text and signatures intact while decoding a
                // separate copy for tool execution. Re-serialization changes
                // escapes, spacing and number spelling in the cached prefix.
                let mut replay = json!({
                    "id": id, "type": "function",
                    "function": {"name": name, "arguments": raw_arguments}
                });
                if let Some(extra) = call.get("extra_content").filter(|v| !v.is_null()) {
                    replay["extra_content"] = extra.clone();
                }
                replay_calls.push(replay);
            }
            if !parsed.is_empty() {
                let mut assistant_message = json!({
                    "role": "assistant",
                    "content": message.get("content").cloned().unwrap_or(Value::Null),
                    "tool_calls": replay_calls
                });
                for field in ["reasoning", "reasoning_content", "reasoning_details"] {
                    if let Some(value) = message.get(field).filter(|v| !v.is_null()) {
                        assistant_message[field] = value.clone();
                    }
                }
                // Fresh reasoning from this response gets a replay sidecar.
                // The wire policy can then distinguish it from older history
                // carrying only another provider's internal reasoning field.
                if assistant_message.get("reasoning_content").is_none() {
                    if let Some(reasoning) = message
                        .get("reasoning")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        assistant_message["reasoning_content"] = json!(reasoning);
                    }
                }
                return Step::ToolCalls {
                    calls: parsed,
                    assistant_message,
                };
            }
        }
    }
    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
    // Structured refusals have no visible content on some chat endpoints.
    // Promote the explanation only when it is the sole usable payload, as in
    // ChatCompletionsTransport.normalize_response. Tool calls already returned
    // above; a refusal annotation must never discard a usable answer.
    let content = if content
        .trim_matches(crate::python_value::python_whitespace)
        .is_empty()
    {
        message
            .get("refusal")
            .and_then(Value::as_str)
            .filter(|text| {
                !text
                    .trim_matches(crate::python_value::python_whitespace)
                    .is_empty()
            })
            .unwrap_or(content)
    } else {
        content
    };
    Step::Final(content.to_owned())
}

/// Run the tool loop for one user message with structured content, streaming the outcome as events.
/// `history` seeds the message list with prior turns (user/assistant/system).
pub async fn run_tool_loop_with_content(
    model: &dyn ChatModel,
    tools: &[Arc<dyn Tool>],
    history: &[crate::session_db::HistoryMessage],
    user_content: &Value,
    events: &mpsc::Sender<StreamEvent>,
    max_iters: usize,
) -> Result<()> {
    let tool_specs: Vec<Value> = tools.iter().map(|t| tool_spec_json(&t.spec())).collect();
    let mut messages = crate::native_agent::build_messages_with_content(history, user_content);

    // Correlation is scoped to the whole turn, including later tool rounds.
    let mut tool_index = 0_i64;
    for _ in 0..max_iters {
        match model.step(&messages, &tool_specs).await? {
            Step::Final(text) => {
                if !text.is_empty() {
                    let _ = events.send(StreamEvent::MessageChunk { text }).await;
                }
                let _ = events.send(StreamEvent::MessageStop { final_: true }).await;
                return Ok(());
            }
            Step::ToolCalls {
                calls,
                assistant_message,
            } => {
                messages.push(assistant_message);
                for call in calls {
                    let _ = events
                        .send(StreamEvent::ToolCallChunk {
                            tool_name: call.name.clone(),
                            preview: None,
                            args: call.arguments.as_object().map(|args| {
                                args.iter()
                                    .map(|(key, value)| (key.clone(), value.clone()))
                                    .collect()
                            }),
                            index: tool_index,
                        })
                        .await;
                    // Exclude downstream event-channel backpressure from the
                    // reported execution time. Instant survives wall-clock jumps.
                    let started = std::time::Instant::now();
                    let (content, ok) = match tools.iter().find(|t| t.spec().name == call.name) {
                        Some(tool) => match tool.call(&call.arguments) {
                            Ok(out) => (out, true),
                            Err(e) => (format!("tool error: {e}"), false),
                        },
                        None => (format!("unknown tool: {}", call.name), false),
                    };
                    let _ = events
                        .send(StreamEvent::ToolCallFinished {
                            tool_name: call.name.clone(),
                            duration: started.elapsed().as_secs_f64(),
                            ok,
                            index: tool_index,
                        })
                        .await;
                    tool_index += 1;
                    let timestamp =
                        json!(chrono::Utc::now().timestamp_micros() as f64 / 1_000_000.0);
                    messages.push(crate::tool_result::build(
                        &call.name,
                        &json!(content),
                        &json!(call.id),
                        &timestamp,
                        None,
                    ));
                }
            }
        }
    }

    warn!(
        max_iters,
        "native tool loop hit iteration cap without a final answer"
    );
    let _ = events
        .send(StreamEvent::MessageChunk {
            text: "(stopped: tool loop exceeded its step limit)".into(),
        })
        .await;
    let _ = events.send(StreamEvent::MessageStop { final_: true }).await;
    Err(Error::Other("tool loop exceeded max iterations".into()))
}

/// Run the tool loop for one user message, streaming the outcome as events.
/// Kept as a wrapper over [`run_tool_loop_with_content`] for callers.
pub async fn run_tool_loop(
    model: &dyn ChatModel,
    tools: &[Arc<dyn Tool>],
    history: &[crate::session_db::HistoryMessage],
    user_text: &str,
    events: &mpsc::Sender<StreamEvent>,
    max_iters: usize,
) -> Result<()> {
    run_tool_loop_with_content(
        model,
        tools,
        history,
        &Value::String(user_text.to_string()),
        events,
        max_iters,
    )
    .await
}

// ---------------------------------------------------------------------------
// A couple of dependency-free built-in tools, useful for a first native turn.
// ---------------------------------------------------------------------------

/// Returns the current Unix time in seconds. No external dependencies.
pub struct CurrentTimeTool;

impl Tool for CurrentTimeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "current_time".into(),
            description: "Get the current time as Unix epoch seconds.".into(),
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        }
    }

    fn call(&self, _args: &Value) -> Result<String> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(secs.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stub model that returns a scripted sequence of steps, one per call.
    struct ScriptedModel {
        steps: Mutex<std::collections::VecDeque<Step>>,
        seen_tools: Mutex<bool>,
    }

    #[async_trait]
    impl ChatModel for ScriptedModel {
        async fn step(&self, messages: &[Value], tools: &[Value]) -> Result<Step> {
            // Record that tool specs are being passed through.
            if !tools.is_empty() {
                *self.seen_tools.lock().unwrap() = true;
            }
            // After the first step, the messages must include the tool result.
            let _ = messages;
            Ok(self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Step::Final(String::new())))
        }
    }

    fn collect(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn tool_step(calls: Vec<ToolCall>) -> Step {
        let assistant_message = assistant_tool_calls_msg(&calls);
        Step::ToolCalls {
            calls,
            assistant_message,
        }
    }

    #[test]
    fn replay_keeps_wire_arguments_while_execution_receives_decoded_values() {
        let raw = r#"{ "letter": "\u0061", "number": 1.00 }"#;
        let message = json!({"role": "assistant", "content": "Checking", "tool_calls": [{
            "id": "c", "type": "function", "function": {"name": "current_time", "arguments": raw},
            "extra_content": {"google": {"thought_signature": "signed"}}
        }]});
        let Step::ToolCalls {
            calls,
            assistant_message,
        } = parse_message_step(&message)
        else {
            panic!("expected tool step")
        };
        assert_eq!(calls[0].arguments["letter"], "a");
        assert_eq!(calls[0].arguments["number"].as_f64(), Some(1.0));
        assert_eq!(assistant_message, message);
    }

    #[test]
    fn refusal_payload_selection_matches_python() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/refusal-goldens.json")).unwrap();
        for case in cases {
            let step = parse_message_step(&case["message"]);
            if case["tool_calls"] == true {
                assert!(matches!(step, Step::ToolCalls { .. }), "{case}");
            } else {
                assert_eq!(
                    step,
                    Step::Final(case["content"].as_str().unwrap().into()),
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn parses_tool_calls_and_final() {
        let msg = json!({
            "role": "assistant", "content": null,
            "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": { "name": "current_time", "arguments": "{}" }
            }]
        });
        assert_eq!(
            parse_message_step(&msg),
            tool_step(vec![ToolCall {
                id: "call_1".into(),
                name: "current_time".into(),
                arguments: json!({}),
            }])
        );
        let final_msg = json!({ "content": "the answer" });
        assert_eq!(
            parse_message_step(&final_msg),
            Step::Final("the answer".into())
        );
    }

    #[tokio::test]
    async fn tool_events_correlate_repeated_calls_across_iterations() {
        struct TimedTool;
        impl Tool for TimedTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "timed".into(),
                    description: "test".into(),
                    parameters: json!({"type":"object"}),
                }
            }
            fn call(&self, args: &Value) -> Result<String> {
                std::thread::sleep(std::time::Duration::from_millis(2));
                if args["fail"] == true {
                    Err(Error::Other("test failure".into()))
                } else {
                    Ok("ok".into())
                }
            }
        }
        let call = |id: &str, fail: bool| ToolCall {
            id: id.into(),
            name: "timed".into(),
            arguments: json!({"fail":fail}),
        };
        let model = RecordingModel {
            steps: Mutex::new(
                vec![
                    tool_step(vec![call("a", false), call("b", true)]),
                    tool_step(vec![call("c", false)]),
                    Step::Final("done".into()),
                ]
                .into(),
            ),
            recorded_messages: Mutex::new(Vec::new()),
        };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(TimedTool)];
        // Reusing a model/client must not carry the event counter into a new turn.
        for _ in 0..2 {
            if model.steps.lock().unwrap().is_empty() {
                *model.steps.lock().unwrap() = vec![
                    tool_step(vec![call("a", false), call("b", true)]),
                    tool_step(vec![call("c", false)]),
                    Step::Final("done".into()),
                ]
                .into();
            }
            let (tx, rx) = mpsc::channel(16);
            run_tool_loop(&model, &tools, &[], "run", &tx, 4)
                .await
                .unwrap();
            drop(tx);
            let events = collect(rx);
            for (index, ok) in [true, false, true].into_iter().enumerate() {
                match &events[index * 2] {
                    StreamEvent::ToolCallChunk {
                        index: actual,
                        args,
                        tool_name,
                        ..
                    } => {
                        assert_eq!(*actual, index as i64);
                        assert_eq!(tool_name, "timed");
                        assert_eq!(args.as_ref().unwrap()["fail"], !ok);
                    }
                    other => panic!("expected tool start, got {other:?}"),
                }
                match &events[index * 2 + 1] {
                    StreamEvent::ToolCallFinished {
                        index: actual,
                        duration,
                        ok: actual_ok,
                        ..
                    } => {
                        assert_eq!(*actual, index as i64);
                        assert_eq!(*actual_ok, ok);
                        assert!(*duration >= 0.002, "tool execution must be timed");
                    }
                    other => panic!("expected tool finish, got {other:?}"),
                }
            }
        }
    }

    #[tokio::test]
    async fn loop_runs_a_tool_then_finalizes() {
        let model = ScriptedModel {
            steps: Mutex::new(
                vec![
                    tool_step(vec![ToolCall {
                        id: "c1".into(),
                        name: "current_time".into(),
                        arguments: json!({}),
                    }]),
                    Step::Final("done".into()),
                ]
                .into(),
            ),
            seen_tools: Mutex::new(false),
        };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CurrentTimeTool)];
        let (tx, rx) = mpsc::channel(32);
        run_tool_loop(&model, &tools, &[], "what time is it?", &tx, 8)
            .await
            .unwrap();
        drop(tx);

        assert!(
            *model.seen_tools.lock().unwrap(),
            "tool specs were passed to the model"
        );
        let events = collect(rx);
        // Expect: ToolCallChunk, ToolCallFinished(ok), MessageChunk("done"), MessageStop.
        assert!(matches!(events[0], StreamEvent::ToolCallChunk { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::ToolCallFinished { ok: true, .. }
        ));
        assert!(matches!(&events[2], StreamEvent::MessageChunk { text } if text == "done"));
        assert!(matches!(
            events[3],
            StreamEvent::MessageStop { final_: true }
        ));
    }

    #[tokio::test]
    async fn unknown_tool_reports_error_but_continues() {
        let model = ScriptedModel {
            steps: Mutex::new(
                vec![
                    tool_step(vec![ToolCall {
                        id: "c1".into(),
                        name: "does_not_exist".into(),
                        arguments: json!({}),
                    }]),
                    Step::Final("recovered".into()),
                ]
                .into(),
            ),
            seen_tools: Mutex::new(false),
        };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CurrentTimeTool)];
        let (tx, rx) = mpsc::channel(32);
        run_tool_loop(&model, &tools, &[], "call a bad tool", &tx, 8)
            .await
            .unwrap();
        drop(tx);
        let events = collect(rx);
        // The unknown tool is reported not-ok, then the loop still finalizes.
        assert!(matches!(
            events[1],
            StreamEvent::ToolCallFinished { ok: false, .. }
        ));
        assert!(matches!(&events[2], StreamEvent::MessageChunk { text } if text == "recovered"));
    }

    #[tokio::test]
    async fn iteration_cap_is_enforced() {
        // A model that only ever asks for tools would loop forever; the cap stops it.
        let steps: std::collections::VecDeque<Step> = std::iter::repeat_with(|| {
            tool_step(vec![ToolCall {
                id: "c".into(),
                name: "current_time".into(),
                arguments: json!({}),
            }])
        })
        .take(10)
        .collect();
        let model = ScriptedModel {
            steps: Mutex::new(steps),
            seen_tools: Mutex::new(false),
        };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CurrentTimeTool)];
        let (tx, _rx) = mpsc::channel(64);
        let result = run_tool_loop(&model, &tools, &[], "loop", &tx, 3).await;
        assert!(result.is_err());
    }

    /// Stub model that records messages passed to step across rounds.
    struct RecordingModel {
        steps: Mutex<std::collections::VecDeque<Step>>,
        recorded_messages: Mutex<Vec<Vec<Value>>>,
    }

    #[async_trait]
    impl ChatModel for RecordingModel {
        async fn step(&self, messages: &[Value], _tools: &[Value]) -> Result<Step> {
            self.recorded_messages
                .lock()
                .unwrap()
                .push(messages.to_vec());
            Ok(self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Step::Final(String::new())))
        }
    }

    #[tokio::test]
    async fn structured_current_content_and_past_content_unchanged_through_tool_rounds() {
        use crate::session_db::HistoryMessage;

        let history = vec![
            HistoryMessage {
                role: "system".into(),
                content: "system prompt".into(),
            },
            HistoryMessage {
                role: "user".into(),
                content: "\0json:[{\"type\":\"text\",\"text\":\"prior image description\"}]".into(),
            },
            HistoryMessage {
                role: "assistant".into(),
                content: "understood".into(),
            },
        ];

        let structured_user_content = json!([
            {"type": "text", "text": "what is the current time?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,abcd"}}
        ]);

        let model = RecordingModel {
            steps: Mutex::new(
                vec![
                    tool_step(vec![ToolCall {
                        id: "call_time_1".into(),
                        name: "current_time".into(),
                        arguments: json!({}),
                    }]),
                    Step::Final("the time has been checked".into()),
                ]
                .into(),
            ),
            recorded_messages: Mutex::new(Vec::new()),
        };

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CurrentTimeTool)];
        let (tx, rx) = mpsc::channel(32);

        run_tool_loop_with_content(&model, &tools, &history, &structured_user_content, &tx, 8)
            .await
            .unwrap();
        drop(tx);

        let recorded = model.recorded_messages.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "model was called for two rounds");

        // Round 1 checks:
        // [0] system history
        // [1] prefix-decoded user history
        // [2] assistant history
        // [3] current user turn with structured content
        assert_eq!(recorded[0].len(), 4);
        assert_eq!(recorded[0][0]["role"], "system");
        assert_eq!(recorded[0][0]["content"], "system prompt");
        assert_eq!(recorded[0][1]["role"], "user");
        assert_eq!(
            recorded[0][1]["content"],
            json!([{"type": "text", "text": "prior image description"}])
        );
        assert_eq!(recorded[0][2]["role"], "assistant");
        assert_eq!(recorded[0][2]["content"], "understood");
        assert_eq!(recorded[0][3]["role"], "user");
        assert_eq!(recorded[0][3]["content"], structured_user_content);
        assert!(recorded[0][3]["content"].is_array());

        // Round 2 checks:
        // Prior messages (0..4) are strictly unchanged byte-stable across tool rounds
        assert_eq!(recorded[1].len(), 6);
        assert_eq!(
            &recorded[1][..4],
            &recorded[0][..],
            "past history and structured current content remain unchanged across tool rounds"
        );
        // Appended assistant tool calls and tool result
        assert_eq!(recorded[1][4]["role"], "assistant");
        assert_eq!(recorded[1][4]["tool_calls"][0]["id"], "call_time_1");
        assert_eq!(recorded[1][5]["role"], "tool");
        assert_eq!(recorded[1][5]["tool_call_id"], "call_time_1");

        // Structured current content still completely intact in round 2
        assert_eq!(recorded[1][3]["content"], structured_user_content);

        let events = collect(rx);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], StreamEvent::ToolCallChunk { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::ToolCallFinished { ok: true, .. }
        ));
        assert!(
            matches!(&events[2], StreamEvent::MessageChunk { text } if text == "the time has been checked")
        );
        assert!(matches!(
            events[3],
            StreamEvent::MessageStop { final_: true }
        ));
    }
}
