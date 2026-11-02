use serde::{Deserialize, Serialize};

/*-- Secret --------------------------------------------------------------------*/

/// Wrapper for a config field that holds sensitive data (API keys, tokens, etc).
///
/// The point of this type is to let the *type system* mark a field as sensitive,
/// rather than guessing from the field name. Its `JsonSchema` impl tags the
/// generated schema with `"format": "password"`, which schema-driven prompt UIs
/// (see `utils::schema_prompt`) use to decide whether to mask input -- no name
/// matching involved.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Secret(pub(crate) String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(\"****\")")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl schemars::JsonSchema for Secret {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Secret".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = <String as schemars::JsonSchema>::json_schema(generator);
        schema.insert("format".to_string(), "password".into());
        schema
    }
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(schemars::JsonSchema)]
    struct HasSecret {
        #[allow(unused)] // Used for schema inspection
        api_key: Option<Secret>,
    }

    #[test]
    fn secret_schema_is_marked_as_password_format() {
        let schema = schemars::schema_for!(HasSecret);

        // `Option<Secret>` renders as `{"anyOf": [{"$ref": "#/$defs/Secret"}, {"type":
        // "null"}]}` at the property -- the "password" marker lives on the hoisted
        // `Secret` definition itself, not inlined at the property site. Search the
        // whole schema (properties + $defs) rather than assuming a specific nesting
        // shape, since that's what matters for consumers like `utils::schema_prompt`.
        let as_str = serde_json::to_string(&schema).unwrap();
        assert!(
            as_str.contains("\"password\""),
            "schema should mark api_key as password format: {as_str}"
        );
    }

    #[test]
    fn secret_debug_never_leaks_the_value() {
        let secret = Secret("super-sensitive-value".to_string());
        let debug_str = format!("{secret:?}");
        assert!(!debug_str.contains("super-sensitive-value"));
        assert!(debug_str.contains("****"));
    }

    #[test]
    fn secret_roundtrips_through_json_as_a_plain_string() {
        let secret = Secret("s3cr3t".to_string());
        let value = serde_json::to_value(&secret).unwrap();
        assert_eq!(value, serde_json::json!("s3cr3t"));

        let back: Secret = serde_json::from_value(value).unwrap();
        assert_eq!(back, secret);
    }
}
