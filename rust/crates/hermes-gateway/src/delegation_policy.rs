//! Delegation configuration consumed by native batch validation.
use serde_json::Value;

/// Config overrides the environment even when invalid. Python falls back to
/// ten in that case; it only uses the environment when config is absent/null.
pub fn max_children(config: &Value, environment: Option<&str>) -> usize {
    let configured = &config["delegation"]["max_concurrent_children"];
    let raw = if configured.is_null() {
        environment.map(Value::from).unwrap_or(Value::Null)
    } else {
        configured.clone()
    };
    match crate::python_value::integer(&raw) {
        Some(value) if value.as_i64().is_some_and(|value| value <= 0) => 1,
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(10)
            .max(1),
        None => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn authority_and_floor() {
        assert_eq!(max_children(&json!({}), None), 10);
        assert_eq!(max_children(&json!({}), Some("15")), 15);
        assert_eq!(
            max_children(
                &json!({"delegation":{"max_concurrent_children":"bad"}}),
                Some("15")
            ),
            10
        );
        assert_eq!(
            max_children(
                &json!({"delegation":{"max_concurrent_children":0}}),
                Some("15")
            ),
            1
        );
        assert_eq!(
            max_children(&json!({"delegation":{"max_concurrent_children":2.9}}), None),
            2
        );
    }
}
