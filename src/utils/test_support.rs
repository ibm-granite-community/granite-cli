//! Test doubles for the factory-constructed traits, shared by the tests that
//! build them.
//!
//! Each test module used to define its own: seven copies of one
//! `FakeProvider`, and six model doubles that differed in an instance id, a
//! `ModelType`, a repository string and whether they carried variants. One
//! definition of each lives here, so a change to `ConfigConstructable` or to
//! a domain trait edits one implementation.

// Standard
use std::collections::HashMap;

// Local
use crate::models::{Model, ModelArchitecture, ModelFunction, ModelType, ModelVariant};
use crate::providers::{
    ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError, PullResult,
};
use crate::registry::{ConfigConstructable, ConstructError, Named, Secret};
use crate::utils::ui::Ui;

/*-- public --*/

/// A provider whose connection details and endpoint table the test states
/// outright, so a binding can be checked without a live upstream.
#[derive(Clone, Default)]
pub(crate) struct FakeProvider {
    pub(crate) instance_id: String,
    pub(crate) base_url: String,
    pub(crate) api_key: Option<Secret>,
    pub(crate) verify_ssl: bool,
    pub(crate) api_types: Vec<ApiType>,
    pub(crate) endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
    /// When set, `model_alias` returns this value instead of `None`.
    pub(crate) alias: Option<String>,
}

impl ConfigConstructable for FakeProvider {
    type Config = crate::registry::NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        unimplemented!("not used in tests")
    }
}

impl Named for FakeProvider {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "Fake Provider"
    }
    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        self.endpoints.clone()
    }
    fn supported_api_types(&self) -> Vec<ApiType> {
        self.api_types.clone()
    }
    fn base_url(&self) -> &str {
        &self.base_url
    }
    fn api_key(&self) -> Option<&Secret> {
        self.api_key.as_ref()
    }
    fn verify_ssl(&self) -> bool {
        self.verify_ssl
    }
    fn supported_formats(&self) -> Vec<ModelFormat> {
        vec![]
    }
    fn can_run_model(&self, _variant_format: &str, _variant_precision: &str) -> bool {
        true
    }
    fn custom_headers(&self) -> Option<HashMap<String, Secret>> {
        None
    }
    async fn pull_model(
        &self,
        _model: &crate::models::ModelMetadata,
        _variant: &ModelVariant,
        _ui: &dyn Ui,
    ) -> Result<PullResult, ProviderError> {
        unimplemented!("not used in tests")
    }
    fn model_alias(&self, _model_id: String, _variant: Option<&ModelVariant>) -> Option<String> {
        self.alias.clone()
    }
    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        unimplemented!("not used in tests")
    }
}

/// A model whose catalog answers are fixed, apart from the ones a test
/// decides: which functions it supports, and which variants it reports.
pub(crate) struct FakeModel {
    instance_id: String,
    model_type: ModelType,
    huggingface_repo: String,
    supported_functions: Vec<ModelFunction>,
    variants: Vec<ModelVariant>,
}

impl FakeModel {
    /// A text model under the catalog id `granite-3.1-8b-instruct`, so a
    /// test that configures it names a model the registry knows.
    pub(crate) fn text(supported_functions: Vec<ModelFunction>) -> Self {
        Self {
            instance_id: "granite-3.1-8b-instruct".to_string(),
            model_type: ModelType::Text,
            huggingface_repo: "test/test".to_string(),
            supported_functions,
            variants: Vec::new(),
        }
    }

    /// A vision model under the id `granite-vision-test`, which is what the
    /// vision-mcp bindings carry.
    pub(crate) fn vision(supported_functions: Vec<ModelFunction>) -> Self {
        Self {
            instance_id: "granite-vision-test".to_string(),
            model_type: ModelType::Vision,
            huggingface_repo: "test/test-vision".to_string(),
            supported_functions,
            variants: Vec::new(),
        }
    }

    /// The variants this model reports, which decide the alias a binding
    /// carries. Empty unless a test sets them.
    pub(crate) fn with_variants(mut self, variants: Vec<ModelVariant>) -> Self {
        self.variants = variants;
        self
    }
}

impl ConfigConstructable for FakeModel {
    type Config = crate::registry::NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        unimplemented!("not used in tests")
    }
}

impl Named for FakeModel {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl Model for FakeModel {
    fn family(&self) -> &str {
        "Test"
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn size(&self) -> u64 {
        1
    }
    fn context_length(&self) -> u64 {
        4096
    }
    fn model_type(&self) -> &ModelType {
        &self.model_type
    }
    fn huggingface_repo(&self) -> &str {
        &self.huggingface_repo
    }
    fn native_dtype(&self) -> &str {
        "bfloat16"
    }
    fn architecture(&self) -> &ModelArchitecture {
        unimplemented!("not used in tests")
    }
    fn variants(&self) -> &[ModelVariant] {
        &self.variants
    }
    fn description(&self) -> Option<&str> {
        None
    }
    fn tags(&self) -> &[String] {
        &[]
    }
    fn supported_functions(&self) -> &[ModelFunction] {
        &self.supported_functions
    }
}
