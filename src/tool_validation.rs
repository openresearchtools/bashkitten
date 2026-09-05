//! Pinned ai/src/utils/validation.ts and TypeBox 1.x conversion for the fixed
//! seven built-in schemas (objects, arrays, strings, numbers and booleans).
use crate::tools::{ToolError, tool_definitions};
use serde_json::{Value, json};

fn single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
}
fn prepare_edit(mut value: Value) -> Value {
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    if let Some(edits) = object.get_mut("edits") {
        if let Some(text) = edits.as_str() {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                if parsed.is_array() {
                    *edits = parsed;
                } else if single_edit(&parsed) {
                    *edits = json!([parsed]);
                }
            }
        } else if single_edit(edits) {
            *edits = json!([edits.take()]);
        }
    }
    if object.get("oldText").is_some_and(Value::is_string)
        && object.get("newText").is_some_and(Value::is_string)
    {
        let old = object.remove("oldText").unwrap();
        let new = object.remove("newText").unwrap();
        let mut edits = object
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        edits.push(json!({"oldText":old,"newText":new}));
        object.insert("edits".into(), Value::Array(edits));
    }
    value
}

fn number(value: &str) -> Option<f64> {
    let input = value.trim_matches(crate::ecmascript::whitespace);
    if input.is_empty() || value.eq_ignore_ascii_case("false") {
        return Some(0.0);
    }
    if value.eq_ignore_ascii_case("true") {
        return Some(1.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return crate::ecmascript::radix_number(rest, radix);
        }
    }
    input.parse::<f64>().ok().filter(|n| n.is_finite())
}
fn normalize_and_convert(value: &mut Value, schema: &Value) {
    match schema["type"].as_str().unwrap_or("") {
        "object" => {
            if let Some(object) = value.as_object_mut() {
                let required = schema["required"].as_array();
                for (key, property) in schema["properties"].as_object().unwrap() {
                    if object.get(key) == Some(&Value::Null)
                        && !required.is_some_and(|keys| keys.iter().any(|v| v == key))
                    {
                        object.remove(key);
                    } else if let Some(value) = object.get_mut(key) {
                        normalize_and_convert(value, property);
                    }
                }
            }
        }
        "array" => {
            if let Some(items) = value.as_array_mut() {
                for item in items {
                    normalize_and_convert(item, &schema["items"]);
                }
            }
        }
        "string" => {
            if value.is_null() || value.is_boolean() || value.is_number() {
                *value = Value::String(if let Some(n) = value.as_f64() {
                    crate::ecmascript::number_string(n)
                } else {
                    value.to_string()
                });
            }
        }
        "number" => {
            let n = if let Some(s) = value.as_str() {
                number(s)
            } else if let Some(b) = value.as_bool() {
                Some(if b { 1.0 } else { 0.0 })
            } else if value.is_null() {
                Some(0.0)
            } else {
                None
            };
            if let Some(n) = n {
                *value = json!(n);
            }
        }
        "boolean" => {
            let result = match value {
                Value::Null => Some(false),
                Value::String(s) if s.eq_ignore_ascii_case("true") || s == "1" => Some(true),
                Value::String(s) if s.eq_ignore_ascii_case("false") || s == "0" => Some(false),
                Value::Number(n) if n.as_f64() == Some(0.0) => Some(false),
                Value::Number(n) if n.as_f64() == Some(1.0) => Some(true),
                _ => None,
            };
            if let Some(b) = result {
                *value = json!(b);
            }
        }
        _ => {}
    }
}
fn validate(value: &Value, schema: &Value, path: &str, errors: &mut Vec<String>) {
    let kind = schema["type"].as_str().unwrap();
    let valid = match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        _ => false,
    };
    let display = if path.is_empty() { "root" } else { path };
    if !valid {
        errors.push(format!("  - {display}: must be {kind}"));
        return;
    }
    let child = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    if let Some(object) = value.as_object() {
        let missing: Vec<_> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|key| !object.contains_key(*key))
            .collect();
        if let Some(first) = missing.first() {
            errors.push(format!(
                "  - {}: must have required properties {}",
                child(first),
                missing.join(", ")
            ));
        }
        for (key, property) in schema["properties"].as_object().unwrap() {
            if let Some(value) = object.get(key) {
                validate(value, property, &child(key), errors);
            }
        }
    } else if let Some(items) = value.as_array() {
        for (i, value) in items.iter().enumerate() {
            validate(value, &schema["items"], &child(&i.to_string()), errors);
        }
    }
}

pub fn prepare(name: &str, arguments: Value) -> Result<Value, ToolError> {
    let tool = tool_definitions()
        .into_iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| ToolError::new(format!("Tool {name} not found")))?;
    let prepared = if name == "edit" {
        prepare_edit(arguments)
    } else {
        arguments
    };
    let mut converted = prepared.clone();
    normalize_and_convert(&mut converted, &tool.parameters);
    let mut errors = Vec::new();
    validate(&converted, &tool.parameters, "", &mut errors);
    if errors.is_empty() {
        Ok(converted)
    } else {
        Err(ToolError::new(format!(
            "Validation failed for tool \"{name}\":\n{}\n\nReceived arguments:\n{}",
            errors.join("\n"),
            crate::ecmascript::pretty_json(&prepared)
        )))
    }
}
