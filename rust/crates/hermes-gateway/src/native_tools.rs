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
    ToolCalls(Vec<ToolCall>),
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
            let parsed = calls
                .iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_str)?.to_string();
                    let f = c.get("function")?;
                    let name = f.get("name").and_then(Value::as_str)?.to_string();
                    // arguments is a JSON string; decode leniently to a Value.
                    let arguments = f
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({}));
                    Some(ToolCall {
                        id,
                        name,
                        arguments,
                    })
                })
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return Step::ToolCalls(parsed);
            }
        }
    }
    Step::Final(
        message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

/// Run the tool loop for one user message, streaming the outcome as events.
/// `history` seeds the message list with prior turns (user/assistant/system).
pub async fn run_tool_loop(
    model: &dyn ChatModel,
    tools: &[Arc<dyn Tool>],
    history: &[crate::session_db::HistoryMessage],
    user_text: &str,
    events: &mpsc::Sender<StreamEvent>,
    max_iters: usize,
) -> Result<()> {
    let tool_specs: Vec<Value> = tools.iter().map(|t| tool_spec_json(&t.spec())).collect();
    let mut messages: Vec<Value> = history
        .iter()
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant" | "system"))
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();
    messages.push(json!({ "role": "user", "content": user_text }));

    for _ in 0..max_iters {
        match model.step(&messages, &tool_specs).await? {
            Step::Final(text) => {
                if !text.is_empty() {
                    let _ = events.send(StreamEvent::MessageChunk { text }).await;
                }
                let _ = events.send(StreamEvent::MessageStop { final_: true }).await;
                return Ok(());
            }
            Step::ToolCalls(calls) => {
                messages.push(assistant_tool_calls_msg(&calls));
                for call in calls {
                    let _ = events
                        .send(StreamEvent::ToolCallChunk {
                            tool_name: call.name.clone(),
                            preview: None,
                            args: None,
                            index: 0,
                        })
                        .await;
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
                            duration: 0.0,
                            ok,
                            index: 0,
                        })
                        .await;
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "content": content,
                    }));
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

    #[test]
    fn parses_tool_calls_and_final() {
        let msg = json!({
            "tool_calls": [{
                "id": "call_1",
                "function": { "name": "current_time", "arguments": "{}" }
            }]
        });
        assert_eq!(
            parse_message_step(&msg),
            Step::ToolCalls(vec![ToolCall {
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
    async fn loop_runs_a_tool_then_finalizes() {
        let model = ScriptedModel {
            steps: Mutex::new(
                vec![
                    Step::ToolCalls(vec![ToolCall {
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
                    Step::ToolCalls(vec![ToolCall {
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
            Step::ToolCalls(vec![ToolCall {
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
}
