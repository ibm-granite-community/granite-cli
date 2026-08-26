// Standard
use std::sync::Arc;

// Third Party
use async_trait::async_trait;

// Local
use crate::models::{
    Model, ModelArchitecture, ModelFunction, ModelMetadata, ModelType, ModelVariant,
};
use crate::providers::{
    ApiEndpoint, HealthStatus, ModelFormat, Provider, ProviderError, PullResult,
};
use crate::registry::Secret;
use crate::utils::ui::Ui;

/*-- public --*/

/// A `Model` decorator whose `provider()` points at the shared session proxy
/// instead of the real upstream. Every other method delegates straight to
/// `inner`, so any caller resolving connection details via
/// `model.provider()` gets routed through (and, when a tracker is active,
/// tracked by) the proxy transparently. Unlike the per-model proxy this
/// replaces, wrapping has no side effects and cannot fail -- the caller
/// (`ModelSource::take`) already registered the real route with the proxy
/// before wrapping.
pub struct ProxiedModel {
    inner: Arc<dyn Model>,
    local_base_url: String,
}

impl ProxiedModel {
    pub fn wrap(inner: Arc<dyn Model>, local_base_url: String) -> Self {
        Self {
            inner,
            local_base_url,
        }
    }
}

impl crate::registry::Named for ProxiedModel {
    fn instance_id(&self) -> &str {
        self.inner.instance_id()
    }
}

impl Model for ProxiedModel {
    fn family(&self) -> &str {
        self.inner.family()
    }
    fn version(&self) -> &str {
        self.inner.version()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn context_length(&self) -> u64 {
        self.inner.context_length()
    }
    fn model_type(&self) -> &ModelType {
        self.inner.model_type()
    }
    fn huggingface_repo(&self) -> &str {
        self.inner.huggingface_repo()
    }
    fn native_dtype(&self) -> &str {
        self.inner.native_dtype()
    }
    fn architecture(&self) -> &ModelArchitecture {
        self.inner.architecture()
    }
    fn variants(&self) -> &[ModelVariant] {
        self.inner.variants()
    }
    fn description(&self) -> Option<&str> {
        self.inner.description()
    }
    fn tags(&self) -> &[String] {
        self.inner.tags()
    }
    fn supported_functions(&self) -> &[ModelFunction] {
        self.inner.supported_functions()
    }

    fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
        Ok(Box::new(ProxiedProvider {
            inner: self.inner.provider()?,
            local_base_url: self.local_base_url.clone(),
        }))
    }
}

/*-- private --*/

/// A `Provider` decorator that redirects connection details at the shared
/// session proxy while delegating everything else -- including
/// `model_alias` (so the alias used to register the route and the one
/// `resolve_provider_endpoint` computes later stay consistent) and the real
/// upstream call made by `health_check`/`pull_model` -- to `inner`.
struct ProxiedProvider {
    inner: Box<dyn Provider>,
    local_base_url: String,
}

impl crate::registry::Named for ProxiedProvider {
    fn instance_id(&self) -> &str {
        self.inner.instance_id()
    }
}

#[async_trait]
impl Provider for ProxiedProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn function_endpoints(&self) -> std::collections::HashMap<ModelFunction, Vec<ApiEndpoint>> {
        self.inner.function_endpoints()
    }
    fn supported_api_types(&self) -> Vec<crate::providers::ApiType> {
        self.inner.supported_api_types()
    }
    fn base_url(&self) -> &str {
        &self.local_base_url
    }
    fn api_key(&self) -> Option<&Secret> {
        // The proxy holds the real credential and injects it upstream; the
        // launched process talking to the local proxy never needs to see it.
        None
    }
    fn verify_ssl(&self) -> bool {
        // The local proxy speaks plain HTTP.
        true
    }
    fn supported_formats(&self) -> Vec<ModelFormat> {
        self.inner.supported_formats()
    }
    fn can_run_model(&self, variant_format: &str, variant_precision: &str) -> bool {
        self.inner.can_run_model(variant_format, variant_precision)
    }
    fn model_alias(&self, variant: Option<&ModelVariant>) -> Option<String> {
        self.inner.model_alias(variant)
    }
    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        self.inner.health_check().await
    }
    async fn pull_model(
        &self,
        model: &ModelMetadata,
        variant: &ModelVariant,
        ui: &dyn Ui,
    ) -> Result<PullResult, ProviderError> {
        self.inner.pull_model(model, variant, ui).await
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeProvider {
        base_url: String,
        api_key: Option<Secret>,
    }

    impl crate::registry::Named for FakeProvider {
        fn instance_id(&self) -> &str {
            "so fake!"
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }
        fn function_endpoints(&self) -> std::collections::HashMap<ModelFunction, Vec<ApiEndpoint>> {
            std::collections::HashMap::new()
        }
        fn supported_api_types(&self) -> Vec<crate::providers::ApiType> {
            vec![crate::providers::ApiType::OpenAI]
        }
        fn base_url(&self) -> &str {
            &self.base_url
        }
        fn api_key(&self) -> Option<&Secret> {
            self.api_key.as_ref()
        }
        fn verify_ssl(&self) -> bool {
            true
        }
        fn supported_formats(&self) -> Vec<ModelFormat> {
            vec![]
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not exercised by these tests")
        }
    }

    struct FakeModel {
        provider: FakeProvider,
    }

    impl crate::registry::Named for FakeModel {
        fn instance_id(&self) -> &str {
            "Mr. McFake"
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
            &ModelType::Text
        }
        fn huggingface_repo(&self) -> &str {
            "test/test"
        }
        fn native_dtype(&self) -> &str {
            "bfloat16"
        }
        fn architecture(&self) -> &ModelArchitecture {
            unimplemented!("not exercised by these tests")
        }
        fn variants(&self) -> &[ModelVariant] {
            &[]
        }
        fn description(&self) -> Option<&str> {
            None
        }
        fn tags(&self) -> &[String] {
            &[]
        }
        fn supported_functions(&self) -> &[ModelFunction] {
            &[]
        }
        fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
            Ok(Box::new(self.provider.clone()))
        }
    }

    #[test]
    fn wrap_points_provider_at_local_proxy_and_clears_api_key() {
        let model: Arc<dyn Model> = Arc::new(FakeModel {
            provider: FakeProvider {
                base_url: "https://api.example.com".to_string(),
                api_key: Some(Secret("real-secret".to_string())),
            },
        });

        let wrapped = ProxiedModel::wrap(Arc::clone(&model), "http://127.0.0.1:9999".to_string());

        let provider = wrapped.provider().unwrap();
        assert_eq!(provider.base_url(), "http://127.0.0.1:9999");
        assert!(provider.api_key().is_none());
        assert!(provider.verify_ssl());
    }

    #[test]
    fn metadata_methods_delegate_to_inner() {
        let model: Arc<dyn Model> = Arc::new(FakeModel {
            provider: FakeProvider {
                base_url: "https://api.example.com".to_string(),
                api_key: None,
            },
        });
        let wrapped = ProxiedModel::wrap(model, "http://127.0.0.1:9999".to_string());

        assert_eq!(wrapped.family(), "Test");
        assert_eq!(wrapped.version(), "1.0");
        assert_eq!(wrapped.context_length(), 4096);
        assert_eq!(wrapped.huggingface_repo(), "test/test");
    }
}
