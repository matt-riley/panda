use crate::Mode;
use anyhow::{Result, ensure};
use serde_json::Value;

/// A handler's declared contract. Only read-only handlers can run in plan mode or overlap.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Mutate,
    /// Branch-local state changes require ordered dispatch, including in plan mode.
    Stateful,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Text,
    Json,
    File,
}
#[derive(Clone)]
pub struct ToolDefinition {
    pub schema: Value,
    pub access: Access,
    pub output: OutputKind,
    /// Namespace used for discovery of composable handlers.
    pub namespace: Option<String>,
}
impl ToolDefinition {
    pub fn name(&self) -> &str {
        self.schema["function"]["name"].as_str().unwrap_or_default()
    }
    pub fn allowed(&self, mode: Mode) -> bool {
        self.access != Access::Mutate || mode == Mode::Code
    }
    pub fn composable(&self, mode: Mode) -> bool {
        self.namespace.is_some()
            && (self.access == Access::Read
                || (mode == Mode::Code
                    && self.access == Access::Mutate
                    && self.namespace.as_deref() == Some("workspace")
                    && matches!(self.name(), "write" | "edit")))
    }
    pub fn validate(&self, args: &Value) -> Result<()> {
        validate(&self.schema["function"]["parameters"], args, self.name())
    }
}

/// Validate the JSON Schema vocabulary used by Panda's tool declarations.
pub fn validate(schema: &Value, value: &Value, path: &str) -> Result<()> {
    match schema["type"].as_str() {
        Some("object") => {
            let obj = value
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("{path} expects an object"))?;
            for key in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                ensure!(obj.contains_key(key), "{path} requires {key}");
            }
            for (key, item) in obj {
                if let Some(rule) = schema["properties"].get(key) {
                    validate(rule, item, &format!("{path}.{key}"))?;
                } else {
                    ensure!(
                        schema["additionalProperties"] != false,
                        "{path}: unknown field {key}"
                    );
                }
            }
        }
        Some("string") => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{path} expects a string"))?;
            ensure!(
                s.chars().count() >= schema["minLength"].as_u64().unwrap_or(0) as usize,
                "{path} is too short"
            );
        }
        Some("array") => {
            let items = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{path} expects an array"))?;
            ensure!(
                items.len() >= schema["minItems"].as_u64().unwrap_or(0) as usize,
                "{path} has too few items"
            );
            for item in items {
                validate(&schema["items"], item, path)?;
            }
        }
        Some("integer") => {
            ensure!(
                value.is_i64() || value.is_u64(),
                "{path} expects an integer"
            );
            let n = value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
                .unwrap();
            ensure!(
                schema["minimum"]
                    .as_i64()
                    .is_none_or(|min| n >= i128::from(min))
                    && schema["maximum"]
                        .as_u64()
                        .is_none_or(|max| n <= i128::from(max)),
                "{path} is outside its allowed range"
            );
        }
        Some("boolean") => ensure!(value.is_boolean(), "{path} expects a boolean"),
        Some("number") => ensure!(value.is_number(), "{path} expects a number"),
        Some("null") => ensure!(value.is_null(), "{path} expects null"),
        Some(other) => anyhow::bail!("{path}: unsupported schema type {other}"),
        None => {}
    }
    if let Some(rules) = schema["oneOf"].as_array() {
        ensure!(
            rules
                .iter()
                .filter(|rule| validate(rule, value, path).is_ok())
                .count()
                == 1,
            "{path} requires exactly one argument form"
        );
    }
    Ok(())
}
