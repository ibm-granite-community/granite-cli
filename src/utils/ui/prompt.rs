// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use serde_json::Value;

// Local
use crate::utils::ui::Ui;

use_channel!("PRMPT");

/*-- Public entry point --------------------------------------------------------*/

/// Interactively prompt for a config value matching `schema`, pre-filled with
/// `defaults` and editable field-by-field. Recurses into nested objects and
/// arrays (indented), and masks input for fields whose schema is marked
/// `"format": "password"` (see `registry::Secret`) rather than guessing from
/// field names.
pub fn prompt_from_schema(
    ui: &dyn Ui,
    schema: &schemars::Schema,
    defaults: &Value,
) -> anyhow::Result<Value> {
    let root = serde_json::to_value(schema)?;
    prompt_value(ui, &root, &root, defaults, "", "")
}

/*-- Recursive dispatch ---------------------------------------------------------*/

fn prompt_value(
    ui: &dyn Ui,
    root: &Value,
    node: &Value,
    default: &Value,
    indent: &str,
    label: &str,
) -> anyhow::Result<Value> {
    // Check if optional BEFORE resolve_ref unwraps anyOf
    let is_optional = is_optional_field(node);
    let node = resolve_ref(root, node);
    if let Some(choices) = enum_choices(root, node) {
        return prompt_enum_scalar(ui, root, &choices, default, indent, label);
    }
    alog_channel!(MessageLevel::Debug3, "Prompting for {:#?}", node);
    match get_promptable_type(node).as_deref() {
        Some("object") => prompt_object(ui, root, node, default, indent, label),
        Some("array") => prompt_array(ui, root, node, default, indent, label),
        Some("string") => Ok(prompt_string(ui, node, default, indent, label, is_optional)?),
        Some("integer") | Some("number") => Ok(prompt_number(ui, node, default, indent, label)?),
        Some("boolean") => Ok(prompt_bool(ui, default, indent, label)?),
        _ => Ok(default.clone()),
    }
}

fn prompt_object(
    ui: &dyn Ui,
    root: &Value,
    node: &Value,
    default: &Value,
    indent: &str,
    label: &str,
) -> anyhow::Result<Value> {
    if !indent.is_empty() && !label.is_empty() {
        ui.info(&format!("{indent}{label}:"));
    }

    let mut result = serde_json::Map::new();
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        let child_indent = format!("{indent}  ");
        for (name, prop_schema) in properties {
            let prop_schema = resolve_ref(root, prop_schema);
            if get_promptable_type(prop_schema).is_none()
                && enum_choices(root, prop_schema).is_none()
            {
                // Untyped / enum-keyed map / unresolved $ref -- no generic UI
                // for these; leave absent so serde falls back to the config
                // struct's own defaults.
                continue;
            }
            let prop_default = default.get(name).cloned().unwrap_or(Value::Null);
            let value = prompt_value(ui, root, prop_schema, &prop_default, &child_indent, name)?;
            result.insert(name.clone(), value);
        }
    }
    Ok(Value::Object(result))
}

fn prompt_array(
    ui: &dyn Ui,
    root: &Value,
    node: &Value,
    default: &Value,
    indent: &str,
    label: &str,
) -> anyhow::Result<Value> {
    let Some(items_schema) = node.get("items").map(|v| resolve_ref(root, v)) else {
        return Ok(Value::Array(vec![]));
    };

    // A `Vec<PureUnitEnum>` -- every choice is a plain literal, none carry
    // their own fields -- gets a single one-shot Multi-Select instead of the
    // per-item "Add?" loop below.
    if let Some(choices) = enum_choices(root, items_schema)
        && choices.iter().all(|c| matches!(c, EnumChoice::Literal(_)))
    {
        return prompt_enum_array(ui, &choices, default, indent, label);
    }

    let default_items: Vec<Value> = default.as_array().cloned().unwrap_or_default();
    let mut defaults_iter = default_items.into_iter();
    let child_indent = format!("{indent}  ");
    let mut items = Vec::new();

    loop {
        let next_default = defaults_iter.next();
        let add = ui.confirm(&format!("{indent}Add {label}?"), next_default.is_some())?;
        if !add {
            break;
        }
        let item_default = next_default.unwrap_or_else(|| zero_value_for(items_schema));
        let item = if let Some(choices) = enum_choices(root, items_schema) {
            // Mixed enum (some choices carry their own fields, e.g. an
            // escape-hatch variant) -- one Select per item, recursing into
            // sub-fields for whichever choice is picked.
            prompt_enum_scalar(ui, root, &choices, &item_default, &child_indent, label)?
        } else {
            prompt_value(ui, root, items_schema, &item_default, &child_indent, label)?
        };
        items.push(item);
    }

    Ok(Value::Array(items))
}

/*-- Enum prompts ----------------------------------------------------------------*/

/// One selectable option derived from an enum-shaped schema: a literal value
/// to use as-is (a unit-variant/plain string-enum entry), or a
/// single-property "tagged" object schema to recurse into when chosen (an
/// externally-tagged variant carrying its own fields, e.g. schemars' default
/// representation of a Rust tuple/struct variant).
enum EnumChoice {
    Literal(String),
    Tagged { key: String, schema: Value },
}

impl EnumChoice {
    fn label(&self) -> &str {
        match self {
            EnumChoice::Literal(s) => s,
            EnumChoice::Tagged { key, .. } => key,
        }
    }
}

/// Detects an enum-shaped schema node -- either a plain `{"type": "string",
/// "enum": [...]}` (an all-unit-variant Rust enum) or a `oneOf` mixing that
/// shape with externally-tagged single-property object alternatives (a
/// mixed enum with some data-carrying variants -- schemars' default
/// representation, confirmed against the vendored `schemars_derive` 1.2.1
/// source). Returns `None` for anything else, so callers fall back to
/// today's per-type prompting.
fn enum_choices(root: &Value, node: &Value) -> Option<Vec<EnumChoice>> {
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        let literals: Vec<EnumChoice> = values
            .iter()
            .filter_map(Value::as_str)
            .map(|s| EnumChoice::Literal(s.to_string()))
            .collect();
        return (!literals.is_empty() && literals.len() == values.len()).then_some(literals);
    }
    if let Some(s) = node.get("const").and_then(Value::as_str) {
        return Some(vec![EnumChoice::Literal(s.to_string())]);
    }

    let alternatives = node.get("oneOf").and_then(Value::as_array)?;
    let mut choices = Vec::new();
    for alt in alternatives {
        let alt = resolve_ref(root, alt);
        if let Some(values) = alt.get("enum").and_then(Value::as_array) {
            let strs: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
            if strs.len() != values.len() {
                return None;
            }
            choices.extend(strs.into_iter().map(|s| EnumChoice::Literal(s.to_string())));
            continue;
        }
        // A unit variant carrying its own schemars attributes (most
        // commonly: a doc comment, which becomes a `"description"`) can't
        // be grouped into the shared `enum` array above -- JSON Schema's
        // `enum` keyword has no way to attach a per-value description --
        // so schemars gives it its own `{"type": "string", "const": "..."}`
        // alternative instead. Confirmed against the real schema
        // `ToolName` generates (see `enum_choices_detects_real_tool_name_schema_with_doc_commented_variants`).
        if let Some(s) = alt.get("const").and_then(Value::as_str) {
            choices.push(EnumChoice::Literal(s.to_string()));
            continue;
        }
        let props = alt.get("properties").and_then(Value::as_object)?;
        let required = alt.get("required").and_then(Value::as_array)?;
        if props.len() != 1 || required.len() != 1 {
            return None;
        }
        let (key, sub_schema) = props.iter().next()?;
        if required.first().and_then(Value::as_str) != Some(key.as_str()) {
            return None;
        }
        choices.push(EnumChoice::Tagged {
            key: key.clone(),
            schema: sub_schema.clone(),
        });
    }
    (!choices.is_empty()).then_some(choices)
}

/// Selects one choice. `Tagged` recurses into `prompt_value` for its
/// sub-schema and wraps the result as `{key: value}` -- reuses whatever
/// object/string/etc. prompting that variant's own fields need.
fn prompt_enum_scalar(
    ui: &dyn Ui,
    root: &Value,
    choices: &[EnumChoice],
    default: &Value,
    indent: &str,
    label: &str,
) -> anyhow::Result<Value> {
    let labels: Vec<String> = choices.iter().map(|c| c.label().to_string()).collect();
    let default_idx = default_choice_index(choices, default).unwrap_or(0);
    let chosen = ui.select(&format!("{indent}{label}"), &labels, default_idx)?;
    match &choices[chosen] {
        EnumChoice::Literal(s) => Ok(Value::String(s.clone())),
        EnumChoice::Tagged { key, schema } => {
            let child_indent = format!("{indent}  ");
            let sub_default = default.get(key).cloned().unwrap_or(Value::Null);
            let value = prompt_value(ui, root, schema, &sub_default, &child_indent, key)?;
            Ok(serde_json::json!({ key: value }))
        }
    }
}

/// One-shot Multi-Select over an array whose items are all `Literal`
/// choices (a `Vec<PureUnitEnum>` field), replacing the generic per-item
/// "Add?" loop with a single checklist prompt.
fn prompt_enum_array(
    ui: &dyn Ui,
    choices: &[EnumChoice],
    default: &Value,
    indent: &str,
    label: &str,
) -> anyhow::Result<Value> {
    let labels: Vec<String> = choices.iter().map(|c| c.label().to_string()).collect();
    let default_items: Vec<&str> = default
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let defaults: Vec<bool> = labels
        .iter()
        .map(|l| default_items.contains(&l.as_str()))
        .collect();
    let chosen = ui.multi_select(&format!("{indent}{label}"), &labels, &defaults)?;
    Ok(Value::Array(
        chosen
            .into_iter()
            .map(|i| Value::String(labels[i].clone()))
            .collect(),
    ))
}

/// Finds which choice `default` corresponds to, so the Select can pre-select
/// it: a plain string default matches a `Literal`'s value; a single-key
/// object default (`{key: ...}`) matches a `Tagged` choice's key.
fn default_choice_index(choices: &[EnumChoice], default: &Value) -> Option<usize> {
    if let Some(s) = default.as_str() {
        return choices
            .iter()
            .position(|c| matches!(c, EnumChoice::Literal(l) if l == s));
    }
    if let Some(obj) = default.as_object() {
        let key = obj.keys().next()?;
        return choices
            .iter()
            .position(|c| matches!(c, EnumChoice::Tagged { key: k, .. } if k == key));
    }
    None
}

/*-- Leaf prompts ---------------------------------------------------------------*/

fn prompt_string(
    ui: &dyn Ui,
    node: &Value,
    default: &Value,
    indent: &str,
    label: &str,
    is_optional: bool,
) -> anyhow::Result<Value> {
    let default_str = default.as_str().unwrap_or("").to_string();
    let prompt = format!("{indent}{label}");

    if is_secret_schema(node) {
        let entered = ui.password(&format!("{prompt} (leave blank to keep current)"))?;
        let value = if entered.is_empty() {
            default_str
        } else {
            entered
        };
        Ok(Value::String(value))
    } else {
        let entered = ui.text(&prompt, &default_str)?;
        // For optional fields, treat empty input (when default is also empty) as None
        if is_optional && entered.is_empty() && default_str.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(Value::String(entered))
        }
    }
}

fn prompt_number(
    ui: &dyn Ui,
    node: &Value,
    default: &Value,
    indent: &str,
    label: &str,
) -> anyhow::Result<Value> {
    let is_integer = get_promptable_type(node).as_deref() == Some("integer");
    let default_str = default
        .as_number()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".to_string());

    let entered = ui.text(&format!("{indent}{label}"), &default_str)?;

    let number = if is_integer {
        entered
            .trim()
            .parse::<i64>()
            .map(serde_json::Number::from)
            .map_err(|_| anyhow::anyhow!("'{entered}' is not a valid integer for {label}"))?
    } else {
        entered
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .ok_or_else(|| anyhow::anyhow!("'{entered}' is not a valid number for {label}"))?
    };

    Ok(Value::Number(number))
}

fn prompt_bool(ui: &dyn Ui, default: &Value, indent: &str, label: &str) -> anyhow::Result<Value> {
    let default_bool = default.as_bool().unwrap_or(false);
    let value = ui.confirm(&format!("{indent}{label}"), default_bool)?;
    Ok(Value::Bool(value))
}

/*-- Pure helpers (unit-testable without a terminal) ----------------------------*/

/// True when the schema marks this field as sensitive via `Secret`'s
/// `"format": "password"` marker -- never guessed from a field name.
fn is_secret_schema(node: &Value) -> bool {
    node.get("format").and_then(Value::as_str) == Some("password")
}

/// True when the schema represents an optional field (Option<T>).
///
/// Detects two patterns that indicate optionality:
/// 1. `type: ["string", "null"]` - appears after `resolve_ref` unwraps an `anyOf`
///    and merges it into a type array. This is what we see in `prompt_object`
///    after calling `resolve_ref` on property schemas.
/// 2. `anyOf: [{type: "string"}, {type: "null"}]` - the raw pattern schemars
///    emits for `Option<T>` before any resolution. This makes `is_optional_field`
///    usable on nodes before `resolve_ref` as well.
fn is_optional_field(node: &Value) -> bool {
    // Case 1: type is an array containing "null" (after resolve_ref)
    if let Some(types) = node.get("type").and_then(Value::as_array) {
        if types.iter().any(|t| t.as_str() == Some("null")) {
            return true;
        }
    }

    // Case 2: anyOf with a null variant (before resolve_ref)
    if let Some(variants) = node.get("anyOf").and_then(Value::as_array) {
        if variants.iter().any(|v| v.get("type").and_then(Value::as_str) == Some("null")) {
            return true;
        }
    }

    false
}

/// Extract the promptable type from a schema node. Returns the concrete type
/// string (`"object"`, `"array"`, `"string"`, `"integer"`, `"number"`,
/// `"boolean"`) when the schema represents one, or `None` when it doesn't.
///
/// Handles three schemars representations of `Option<T>`:
/// 1. `"type": "string"` — direct type after `resolve_ref` unwrapping
/// 2. `"type": ["string", "null"]` — type array (some schemars versions)
/// 3. `anyOf` with a single non-null variant — unresolved ref inside anyOf
fn get_promptable_type(node: &Value) -> Option<String> {
    let promptable_types = ["object", "array", "string", "integer", "number", "boolean"];

    // Case 1: `type` is a single string (e.g. `"string"`).
    if let Some(t) = node.get("type").and_then(Value::as_str)
        && promptable_types.contains(&t)
    {
        return Some(t.to_string());
    }

    // Case 2: `type` is an array of strings (e.g. `["string", "null"]`).
    if let Some(types) = node.get("type").and_then(Value::as_array) {
        let non_null: Vec<&str> = types
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|t| *t != "null")
            .collect();
        if non_null.len() == 1 && promptable_types.contains(&non_null[0]) {
            return Some(non_null[0].to_string());
        }
    }

    // Case 3: `anyOf` with exactly one non-null variant means schemars
    // represented an Option<T> and resolve_ref may not have fully
    // unwrapped it (e.g. unresolved $ref inside anyOf).
    if let Some(variants) = node.get("anyOf").and_then(Value::as_array) {
        let mut non_null = variants.iter().filter(|v| {
            v.get("type").and_then(Value::as_str) != Some("null")
                && v.get("type").and_then(Value::as_str).is_some()
        });
        if let (Some(only), None) = (non_null.next(), non_null.next()) {
            return get_promptable_type(only);
        }
    }

    None
}

/// Resolve a schema node to the concrete (object/array/scalar) schema it
/// describes, following two indirections schemars commonly introduces:
///
/// - `{"$ref": "#/$defs/Name"}` (or legacy `#/definitions/Name`) -- a
///   reference to a named, hoisted schema (e.g. any `Option<Secret>` field,
///   since `Secret`'s `JsonSchema` impl gives it a name and schemars hoists
///   named schemas rather than inlining them).
/// - `{"anyOf": [<schema>, {"type": "null"}]}` -- how schemars renders
///   `Option<T>` once `T` is itself a `$ref` (a plain merged nullable type
///   isn't possible across a reference boundary). Resolved to the single
///   non-null variant.
///
/// Both can nest (an `anyOf`'s surviving variant is often itself a `$ref`),
/// so resolution recurses. Returns `node` unchanged if neither pattern
/// matches or the reference can't be resolved.
fn resolve_ref<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        let target = root
            .get("$defs")
            .or_else(|| root.get("definitions"))
            .and_then(|defs| defs.get(name));
        return match target {
            Some(target) => resolve_ref(root, target),
            None => node,
        };
    }

    if let Some(variants) = node.get("anyOf").and_then(Value::as_array) {
        let mut non_null = variants
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"));
        if let (Some(only), None) = (non_null.next(), non_null.next()) {
            return resolve_ref(root, only);
        }
    }

    node
}

/// A type-appropriate empty value, used to seed a fresh array item when no
/// default item remains to prefill it with.
fn zero_value_for(schema: &Value) -> Value {
    match get_promptable_type(schema).as_deref() {
        Some("object") => Value::Object(serde_json::Map::new()),
        Some("array") => Value::Array(vec![]),
        Some("string") => Value::String(String::new()),
        Some("integer") | Some("number") => Value::Number(0.into()),
        Some("boolean") => Value::Bool(false),
        _ => Value::Null,
    }
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_format_is_detected_from_schema_not_field_name() {
        assert!(is_secret_schema(
            &json!({"type": "string", "format": "password"})
        ));
        assert!(!is_secret_schema(&json!({"type": "string"})));
        // A field literally named "api_key" with no format marker is NOT a secret.
        assert!(!is_secret_schema(
            &json!({"type": "string", "title": "api_key"})
        ));
    }

    #[test]
    fn is_optional_field_detects_type_array_with_null() {
        // Type arrays can appear after schema resolution
        assert!(is_optional_field(&json!({"type": ["string", "null"]})));
        assert!(is_optional_field(&json!({"type": ["integer", "null"]})));
        assert!(!is_optional_field(&json!({"type": "string"})));
        assert!(!is_optional_field(&json!({"type": ["string", "integer"]})));
    }

    #[test]
    fn is_optional_field_detects_any_of_with_null_variant() {
        // schemars emits anyOf for Option<T>
        assert!(is_optional_field(&json!({"anyOf": [{"type": "string"}, {"type": "null"}]})));
        assert!(is_optional_field(&json!({"anyOf": [{"type": "integer"}, {"type": "null"}]})));

        // Non-optional patterns
        assert!(!is_optional_field(&json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})));
        assert!(!is_optional_field(&json!({"type": "string"})));
    }

    #[test]
    fn prompt_from_schema_returns_null_for_empty_optional_string() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            command_path: Option<String>,
        }

        let ui = CaptureUi::default();
        // Empty string input for optional field
        ui.text_answers.borrow_mut().push_back("".to_string());

        let schema = schemars::schema_for!(TestConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        // Should be null, not empty string
        assert_eq!(result["command_path"], json!(null));

        // Should deserialize correctly as None
        let config: TestConfig = serde_json::from_value(result).unwrap();
        assert!(config.command_path.is_none());
    }

    #[test]
    fn prompt_from_schema_returns_string_for_non_empty_optional_string() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            command_path: Option<String>,
        }

        let ui = CaptureUi::default();
        // Non-empty string input for optional field
        ui.text_answers.borrow_mut().push_back("/usr/bin/bob".to_string());

        let schema = schemars::schema_for!(TestConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        // Should be the entered string
        assert_eq!(result["command_path"], json!("/usr/bin/bob"));

        // Should deserialize correctly as Some
        let config: TestConfig = serde_json::from_value(result).unwrap();
        assert_eq!(config.command_path, Some("/usr/bin/bob".to_string()));
    }

    #[test]
    fn prompt_from_schema_keeps_default_for_empty_input_on_optional_string() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            command_path: Option<String>,
        }

        let ui = CaptureUi::default();
        // Empty string input, but there's a default value
        ui.text_answers.borrow_mut().push_back("".to_string());

        let schema = schemars::schema_for!(TestConfig);
        let defaults = json!({"command_path": "/existing/path"});
        let result = prompt_from_schema(&ui, &schema, &defaults).unwrap();

        // Should keep the default, not return null
        assert_eq!(result["command_path"], json!(""));

        // Empty string is still a valid value when there was a default
        let config: TestConfig = serde_json::from_value(result).unwrap();
        assert_eq!(config.command_path, Some("".to_string()));
    }

    #[test]
    fn prompt_from_schema_returns_empty_string_for_required_string() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            name: String,
        }

        let ui = CaptureUi::default();
        // Empty string input for required field
        ui.text_answers.borrow_mut().push_back("".to_string());

        let schema = schemars::schema_for!(TestConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        // Should be empty string, not null (required field)
        assert_eq!(result["name"], json!(""));
        
        // Should deserialize correctly as empty string
        let config: TestConfig = serde_json::from_value(result).unwrap();
        assert_eq!(config.name, "");
    }

    #[test]
    fn promptable_types_are_object_array_and_scalars() {
        assert_eq!(
            get_promptable_type(&json!({"type": "object"})),
            Some("object".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": "array"})),
            Some("array".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": "string"})),
            Some("string".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": "integer"})),
            Some("integer".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": "number"})),
            Some("number".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": "boolean"})),
            Some("boolean".to_string())
        );
        assert_eq!(get_promptable_type(&json!({})), None);
        assert_eq!(
            get_promptable_type(&json!({"$ref": "#/$defs/Unresolved"})),
            None
        );
    }

    #[test]
    fn promptable_type_falls_back_to_any_of_for_option_types() {
        // When resolve_ref cannot fully unwrap anyOf (e.g. unresolved $ref
        // inside the non-null variant), get_promptable_type still extracts
        // the single-variant anyOf type.
        assert_eq!(
            get_promptable_type(&json!({"anyOf": [{"type": "string"}, {"type": "null"}]})),
            Some("string".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"anyOf": [{"type": "integer"}, {"type": "null"}]})),
            Some("integer".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"anyOf": [{"type": "boolean"}, {"type": "null"}]})),
            Some("boolean".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"anyOf": [{"type": "object"}, {"type": "null"}]})),
            Some("object".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"anyOf": [{"type": "array"}, {"type": "null"}]})),
            Some("array".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"anyOf": [{"type": "number"}, {"type": "null"}]})),
            Some("number".to_string())
        );
        // Multiple non-null variants are NOT promptable.
        assert_eq!(
            get_promptable_type(
                &json!({"anyOf": [{"type": "string"}, {"type": "integer"}, {"type": "null"}]})
            ),
            None
        );
        // anyOf without a type marker in variants is NOT promptable.
        assert_eq!(
            get_promptable_type(
                &json!({"anyOf": [{"$ref": "#/$defs/Unresolved"}, {"type": "null"}]})
            ),
            None
        );
    }

    #[test]
    fn promptable_type_handles_type_as_array_for_option() {
        // Some schemars versions represent Option<T> as `"type": ["string", "null"]`
        // rather than using anyOf.
        assert_eq!(
            get_promptable_type(&json!({"type": ["string", "null"]})),
            Some("string".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": ["integer", "null"]})),
            Some("integer".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": ["boolean", "null"]})),
            Some("boolean".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": ["number", "null"]})),
            Some("number".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": ["array", "null"]})),
            Some("array".to_string())
        );
        assert_eq!(
            get_promptable_type(&json!({"type": ["object", "null"]})),
            Some("object".to_string())
        );
        // Null-only is NOT promptable.
        assert_eq!(get_promptable_type(&json!({"type": ["null"]})), None);
        // Multiple non-null types are NOT promptable.
        assert_eq!(
            get_promptable_type(&json!({"type": ["string", "integer", "null"]})),
            None
        );
    }

    #[test]
    fn ref_resolves_against_defs() {
        let root = json!({
            "$defs": {
                "Inner": {"type": "object", "properties": {"x": {"type": "integer"}}}
            }
        });
        let node = json!({"$ref": "#/$defs/Inner"});
        let resolved = resolve_ref(&root, &node);
        assert_eq!(resolved.get("type").and_then(Value::as_str), Some("object"));
    }

    #[test]
    fn ref_resolves_against_legacy_definitions() {
        let root = json!({
            "definitions": {
                "Inner": {"type": "string"}
            }
        });
        let node = json!({"$ref": "#/definitions/Inner"});
        let resolved = resolve_ref(&root, &node);
        assert_eq!(resolved.get("type").and_then(Value::as_str), Some("string"));
    }

    #[test]
    fn unresolvable_ref_falls_back_to_node_itself() {
        let root = json!({});
        let node = json!({"$ref": "#/$defs/Missing"});
        let resolved = resolve_ref(&root, &node);
        assert_eq!(resolved, &node);
    }

    #[test]
    fn any_of_option_wrapper_resolves_to_the_non_null_variant() {
        // How schemars renders a plain `Option<T>` where `T` isn't a `$ref`.
        let root = json!({});
        let node = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
        let resolved = resolve_ref(&root, &node);
        assert_eq!(resolved.get("type").and_then(Value::as_str), Some("string"));
    }

    #[test]
    fn any_of_option_wrapper_around_a_ref_resolves_through_both() {
        // How schemars renders `Option<Secret>`: the named `Secret` schema is hoisted
        // into `$defs` and referenced, and `Option<...>` wraps that ref in `anyOf`
        // alongside a null variant, since it can't merge "null" into a `$ref` inline.
        let root = json!({
            "$defs": {
                "Secret": {"type": "string", "format": "password"}
            }
        });
        let node = json!({"anyOf": [{"$ref": "#/$defs/Secret"}, {"type": "null"}]});
        let resolved = resolve_ref(&root, &node);
        assert_eq!(resolved.get("type").and_then(Value::as_str), Some("string"));
        assert!(is_secret_schema(resolved));
        assert_eq!(get_promptable_type(resolved), Some("string".to_string()));
    }

    #[test]
    fn zero_value_matches_schema_type() {
        assert_eq!(zero_value_for(&json!({"type": "string"})), json!(""));
        assert_eq!(zero_value_for(&json!({"type": "integer"})), json!(0));
        assert_eq!(zero_value_for(&json!({"type": "boolean"})), json!(false));
        assert_eq!(zero_value_for(&json!({"type": "array"})), json!([]));
        assert_eq!(zero_value_for(&json!({"type": "object"})), json!({}));
        // Also works with array-style type (Option<T>).
        assert_eq!(
            zero_value_for(&json!({"type": ["string", "null"]})),
            json!("")
        );
        assert_eq!(
            zero_value_for(&json!({"type": ["integer", "null"]})),
            json!(0)
        );
    }

    #[test]
    fn enum_choices_detects_pure_unit_enum() {
        let node = json!({"type": "string", "enum": ["FileRead", "FileWrite"]});
        let choices = enum_choices(&json!({}), &node).unwrap();
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|c| matches!(c, EnumChoice::Literal(_))));
        assert_eq!(choices[0].label(), "FileRead");
        assert_eq!(choices[1].label(), "FileWrite");
    }

    #[test]
    fn enum_choices_detects_mixed_enum_with_tagged_variants() {
        let node = json!({
            "oneOf": [
                {"type": "string", "enum": ["FileRead", "FileWrite"]},
                {"type": "object", "properties": {"Mcp": {"type": "object"}}, "required": ["Mcp"]},
                {"type": "object", "properties": {"Other": {"type": "string"}}, "required": ["Other"]},
            ]
        });
        let choices = enum_choices(&json!({}), &node).unwrap();
        let labels: Vec<&str> = choices.iter().map(EnumChoice::label).collect();
        assert_eq!(labels, vec!["FileRead", "FileWrite", "Mcp", "Other"]);
        assert!(matches!(choices[2], EnumChoice::Tagged { .. }));
        assert!(matches!(choices[3], EnumChoice::Tagged { .. }));
    }

    #[test]
    fn enum_choices_returns_none_for_non_enum_schema() {
        assert!(enum_choices(&json!({}), &json!({"type": "object", "properties": {}})).is_none());
        assert!(enum_choices(&json!({}), &json!({"type": "string"})).is_none());
        assert!(enum_choices(&json!({}), &json!({})).is_none());
    }

    #[test]
    fn enum_choices_returns_none_for_one_of_alternative_that_is_not_single_tagged_property() {
        // A oneOf alternative with more than one property isn't a
        // single-property externally-tagged enum variant.
        let node = json!({
            "oneOf": [
                {"type": "string", "enum": ["FileRead"]},
                {"type": "object", "properties": {"a": {}, "b": {}}, "required": ["a", "b"]},
            ]
        });
        assert!(enum_choices(&json!({}), &node).is_none());
    }

    #[test]
    fn prompt_from_schema_drives_mixed_enum_array_through_select_and_tagged_recursion() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        #[allow(dead_code)]
        enum TestTool {
            FileRead,
            FileWrite,
            Mcp {
                server: String,
                tool: Option<String>,
            },
            Other(String),
        }

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            tools: Vec<TestTool>,
        }

        let ui = CaptureUi::default();
        ui.confirm_answers.borrow_mut().push_back(true); // Add tools item? yes
        ui.select_answers.borrow_mut().push_back(2); // choices[2] == "Mcp"
        ui.text_answers.borrow_mut().push_back("vision".to_string()); // server
        ui.text_answers
            .borrow_mut()
            .push_back("vlm_compare_images".to_string()); // tool
        ui.confirm_answers.borrow_mut().push_back(false); // Add tools item? no more

        let schema = schemars::schema_for!(TestConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        assert_eq!(
            result,
            json!({"tools": [{"Mcp": {"server": "vision", "tool": "vlm_compare_images"}}]})
        );
    }

    #[test]
    fn enum_choices_detects_a_one_of_alternative_shaped_as_const_not_enum() {
        // A unit variant carrying its own schemars attributes (e.g. a doc
        // comment, which becomes a "description") gets its own
        // `{"type": "string", "const": "..."}` alternative rather than
        // being grouped into a shared `enum` array -- JSON Schema's `enum`
        // keyword can't carry a per-value description. This is exactly
        // what `ToolName`'s doc-commented variants (`Search`, `FileSearch`,
        // `Shell`) produce; without this case `enum_choices` bails via `?`
        // on the very first such alternative and returns `None` for the
        // *entire* enum.
        let node = json!({
            "oneOf": [
                {"type": "string", "enum": ["FileRead", "FileWrite"]},
                {"type": "string", "const": "Search", "description": "Content search."},
                {"type": "object", "properties": {"Other": {"type": "string"}}, "required": ["Other"]},
            ]
        });
        let choices = enum_choices(&json!({}), &node).unwrap();
        let labels: Vec<&str> = choices.iter().map(EnumChoice::label).collect();
        assert_eq!(labels, vec!["FileRead", "FileWrite", "Search", "Other"]);
        assert!(matches!(choices[2], EnumChoice::Literal(_)));
    }

    #[test]
    fn enum_choices_detects_the_real_tool_name_schema_end_to_end() {
        // Regression test: an earlier version of `enum_choices` didn't
        // recognize the `const`-shaped alternative above, so it returned
        // `None` for the real `ToolName` type (which has doc comments on
        // some but not all unit variants) even though it worked for
        // hand-written schemas and a doc-comment-free test enum. Exercise
        // the actual production types, not a synthetic schema.
        let schema = schemars::schema_for!(crate::capabilities::SubAgentCapabilityConfig);
        let root = serde_json::to_value(&schema).unwrap();
        let items_schema = root
            .get("properties")
            .and_then(|p| p.get("tools"))
            .map(|v| resolve_ref(&root, v))
            .and_then(|tools| tools.get("items"))
            .map(|v| resolve_ref(&root, v))
            .expect("tools.items present");
        assert!(enum_choices(&root, items_schema).is_some());
    }

    #[test]
    fn prompt_from_schema_drives_the_real_sub_agent_capability_config_end_to_end() {
        use crate::capabilities::SubAgentCapabilityConfig;
        use crate::utils::ui::base::tests::CaptureUi;

        let ui = CaptureUi::default();
        // Fields prompt in alphabetical property order (plain serde_json::Map):
        // description, model_id, prompt, tools.
        ui.text_answers
            .borrow_mut()
            .push_back("Reviews code".to_string());
        ui.text_answers
            .borrow_mut()
            .push_back("granite-3.1-8b-instruct".to_string());
        ui.text_answers
            .borrow_mut()
            .push_back("You are a meticulous code reviewer.".to_string());
        ui.confirm_answers.borrow_mut().push_back(true); // Add tools? yes
        // Flattened choice order: FileRead, FileWrite, FileEdit, WebFetch,
        // WebSearch (the shared enum group, in that order), then each
        // doc-commented unit variant's own const alternative -- Search,
        // FileSearch, Shell -- then Mcp, Other. Index 5 == "Search".
        ui.select_answers.borrow_mut().push_back(5);
        ui.confirm_answers.borrow_mut().push_back(false); // Add tools? no more

        let schema = schemars::schema_for!(SubAgentCapabilityConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        assert_eq!(result["description"], "Reviews code");
        assert_eq!(result["model_id"], "granite-3.1-8b-instruct");
        assert_eq!(result["prompt"], "You are a meticulous code reviewer.");
        assert_eq!(result["tools"], json!(["Search"]));

        // Deserializes back into the real config type.
        let config: SubAgentCapabilityConfig = serde_json::from_value(result).unwrap();
        assert_eq!(config.tools, vec![crate::capabilities::ToolName::Search]);
    }

    #[test]
    fn prompt_from_schema_drives_mixed_enum_array_through_select_of_a_plain_literal() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        #[allow(dead_code)]
        enum TestTool {
            FileRead,
            FileWrite,
            Other(String),
        }

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            tools: Vec<TestTool>,
        }

        let ui = CaptureUi::default();
        ui.confirm_answers.borrow_mut().push_back(true); // Add tools item? yes
        ui.select_answers.borrow_mut().push_back(0); // choices[0] == "FileRead"
        ui.confirm_answers.borrow_mut().push_back(false); // Add tools item? no more

        let schema = schemars::schema_for!(TestConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        assert_eq!(result, json!({"tools": ["FileRead"]}));
    }

    #[test]
    fn prompt_from_schema_multi_selects_a_pure_unit_enum_array_in_one_shot() {
        use crate::utils::ui::base::tests::CaptureUi;

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        #[allow(dead_code)]
        enum TestTool {
            FileRead,
            FileWrite,
            Shell,
        }

        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        struct TestConfig {
            tools: Vec<TestTool>,
        }

        let ui = CaptureUi::default();
        // A single multi_select call picking FileRead (0) and Shell (2) -- no
        // "Add another?" confirm loop for a pure (escape-hatch-free) enum.
        ui.multi_select_answers.borrow_mut().push_back(vec![0, 2]);

        let schema = schemars::schema_for!(TestConfig);
        let result = prompt_from_schema(&ui, &schema, &json!({})).unwrap();

        assert_eq!(result, json!({"tools": ["FileRead", "Shell"]}));
        assert_eq!(ui.multi_select_prompts.borrow().len(), 1);
        assert!(ui.confirm_prompts.borrow().is_empty());
    }
}
