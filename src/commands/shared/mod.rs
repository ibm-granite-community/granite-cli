//! Behaviour shared by several command modules, for what is not itself a
//! top-level sub-command.

pub(crate) mod discovery;
pub(crate) mod remediation;

/*-- public --------------------------------------------------------------------*/

/// The detail fields for an instance's `config` object, one per key, named
/// `Config: <key>`.
///
/// Each key gets its own field name because the JSON backend keys a detail's
/// fields by name, so fields sharing a name overwrite each other. A string
/// value is shown without JSON quotes. Any other value is shown as JSON.
pub(crate) fn config_fields(config: &serde_json::Value) -> Vec<(String, String)> {
    let Some(object) = config.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .map(|(key, value)| {
            let shown = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            (format!("Config: {key}"), shown)
        })
        .collect()
}

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_config_key_is_its_own_field_with_strings_unquoted() {
        let fields = config_fields(&serde_json::json!({
            "base_url": "http://localhost:11434",
            "api_key": null,
            "timeout_secs": 10,
            "verify_ssl": true,
        }));

        assert_eq!(
            fields,
            vec![
                ("Config: api_key".to_string(), "null".to_string()),
                (
                    "Config: base_url".to_string(),
                    "http://localhost:11434".to_string()
                ),
                ("Config: timeout_secs".to_string(), "10".to_string()),
                ("Config: verify_ssl".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn a_config_that_is_not_an_object_has_no_fields() {
        assert!(config_fields(&serde_json::Value::Null).is_empty());
    }
}
