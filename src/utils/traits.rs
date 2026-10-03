/*-- Searchable Trait ---------------------------------------------------------*/

/// Declares which string fields on a metadata struct participate in search.
///
/// The command layer calls `search_fields()` rather than accessing individual
/// fields — adding a new searchable field is a one-line change here, not in
/// the command. The same trait can be implemented for `ProviderMetadata` and
/// `CapabilityMetadata` to enable `provider search` / `capability search`.
pub trait Searchable {
    /// All string values that should be matched against a search query.
    /// The item ID is matched separately by the caller (it is the registry
    /// key, not a field on the metadata struct).
    fn search_fields(&self) -> Vec<&str>;

    /// Whether any search field contains `query`, ignoring case.
    /// `query` must already be lowercase.
    fn matches_query(&self, query: &str) -> bool {
        self.search_fields()
            .iter()
            .any(|f| f.to_lowercase().contains(query))
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod searchable_tests {
    use super::*;
    use crate::models::{LayerKind, LayerTypeCount, ModelArchitecture};
    use crate::models::{ModelMetadata, ModelType};

    fn metadata(family: &str, description: Option<&str>, tags: Vec<&str>) -> ModelMetadata {
        ModelMetadata {
            family: family.to_string(),
            version: "1.0".to_string(),
            size: 8_000_000_000,
            context_length: 4096,
            model_type: ModelType::Text,
            huggingface_repo: "ibm-granite/test".to_string(),
            native_dtype: "bfloat16".to_string(),
            architecture: ModelArchitecture {
                num_hidden_layers: 1,
                hidden_size: 1,
                num_attention_heads: 1,
                num_key_value_heads: 1,
                head_dim: 1,
                layer_types: vec![LayerTypeCount {
                    kind: LayerKind::FullAttention,
                    count: 1,
                }],
            },
            variants: vec![],
            description: description.map(String::from),
            tags: tags.into_iter().map(String::from).collect(),
            supported_functions: vec![],
        }
    }

    #[test]
    fn searchable_fields_includes_family() {
        let m = metadata("Granite 3.1", None, vec![]);
        assert!(m.search_fields().contains(&"Granite 3.1"));
    }

    #[test]
    fn searchable_fields_includes_description_when_present() {
        let m = metadata("Granite 3.1", Some("A text model"), vec![]);
        assert!(m.search_fields().contains(&"A text model"));
    }

    #[test]
    fn searchable_fields_omits_description_when_absent() {
        let m = metadata("Granite 3.1", None, vec![]);
        assert_eq!(m.search_fields().len(), 1);
    }

    #[test]
    fn searchable_fields_includes_tags() {
        let m = metadata("Granite 3.1", None, vec!["instruct", "chat"]);
        let fields = m.search_fields();
        assert!(fields.contains(&"instruct"));
        assert!(fields.contains(&"chat"));
    }

    #[test]
    fn matches_query_matches_tag_only() {
        let m = metadata("Granite 3.1", None, vec!["vision"]);
        assert!(m.matches_query("vision"));
    }

    #[test]
    fn matches_query_ignores_field_case() {
        let m = metadata("Granite 3.1", Some("A Vision model"), vec![]);
        assert!(m.matches_query("vision"));
    }

    #[test]
    fn matches_query_no_match() {
        let m = metadata("Granite 3.1", Some("A text model"), vec!["chat"]);
        assert!(!m.matches_query("vision"));
    }

    #[test]
    fn matches_query_matches_family() {
        let m = metadata("Granite 3.1", None, vec![]);
        assert!(m.matches_query("granite"));
    }
}
