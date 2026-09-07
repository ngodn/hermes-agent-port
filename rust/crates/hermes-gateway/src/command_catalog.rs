//! Registry metadata from the Python reference. Recognition is distinct from
//! handler availability; the dispatcher still owns implemented commands.

use serde_json::Value;
use std::sync::OnceLock;

fn commands() -> &'static [Value] {
    static COMMANDS: OnceLock<Vec<Value>> = OnceLock::new();
    COMMANDS.get_or_init(|| {
        serde_json::from_str(include_str!("../data/commands.json"))
            .expect("generated command catalog")
    })
}

/// Include config-gated commands in recognition, as Python does. The eventual
/// handler checks its config gate. Plugin registration is a later runtime slice.
pub fn gateway_knows(name: &str) -> bool {
    commands().iter().any(|command| {
        (command["cli_only"] != true
            || crate::python_value::truthy(&command["gateway_config_gate"]))
            && (command["name"] == name
                || command["aliases"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == name)))
    })
}
