// Standard
use std::sync::Arc;

// Third Party
use async_trait::async_trait;

// Local
use crate::models::{ModelFunction, ModelMetadata, ModelVariant};
use crate::providers::{
    ApiEndpoint, HealthStatus, ModelFormat, Provider, ProviderError, PullResult,
};
use crate::registry::Secret;
use crate::utils::ui::Ui;

/*-- public --*/

/// A `Provider` decorator that redirects connection details at the shared
/// session proxy while delegating everything else -- including
/// `model_alias` (so the alias used to register the route and the one
/// `resolve_provider_endpoint` computes later stay consistent) and the real
/// upstream call made by `health_check`/`pull_model` -- to `inner`.
pub(crate) struct ProxiedProvider {
    inner: Arc<dyn Provider>,
    local_base_url: String,
}

impl ProxiedProvider {
    /// Point `inner`'s connection details at the proxy listening on
    /// `local_base_url`. Wrapping has no side effects and cannot fail: the
    /// route to the real upstream is registered by the launch path, from the
    /// unwrapped provider, before anything is wrapped.
    pub(crate) fn wrap(inner: Arc<dyn Provider>, local_base_url: String) -> Self {
        Self {
            inner,
            local_base_url,
        }
    }
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
    fn model_alias(&self, model_id: String, variant: Option<&ModelVariant>) -> Option<String> {
        self.inner.model_alias(model_id, variant)
    }
    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        self.inner.health_check().await
    }

    fn custom_headers(&self) -> Option<std::collections::HashMap<String, Secret>> {
        self.inner.custom_headers()
    }
    async fn pull_model(
        &self,
        model: &ModelMetadata,
        variant: &ModelVariant,
        ui: &dyn Ui,
    ) -> Result<PullResult, ProviderError> {
        self.inner.pull_model(model, variant, ui).await
    }

    fn discover_models(&self) -> bool {
        self.inner.discover_models()
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ProviderModel>, ProviderError> {
        // Reads the real upstream through `inner`'s own base URL, not the
        // session proxy this wrapper points at.
        self.inner.list_models().await
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
        fn custom_headers(&self) -> Option<std::collections::HashMap<String, Secret>> {
            None
        }
        fn supported_formats(&self) -> Vec<ModelFormat> {
            vec![]
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not exercised by these tests")
        }
    }

    #[test]
    fn wrap_points_connection_details_at_local_proxy_and_clears_api_key() {
        let real: Arc<dyn Provider> = Arc::new(FakeProvider {
            base_url: "https://api.example.com".to_string(),
            api_key: Some(Secret("real-secret".to_string())),
        });

        let wrapped = ProxiedProvider::wrap(Arc::clone(&real), "http://127.0.0.1:9999".to_string());

        assert_eq!(wrapped.base_url(), "http://127.0.0.1:9999");
        assert!(
            wrapped.api_key().is_none(),
            "the launched process talks to the proxy and never sees the real credential"
        );
        assert!(wrapped.verify_ssl());
        assert_eq!(
            real.base_url(),
            "https://api.example.com",
            "wrapping must leave the real provider's own details alone, since the \
             launch path reads them to register the route"
        );
    }

    #[test]
    fn everything_but_connection_details_delegates_to_inner() {
        let real: Arc<dyn Provider> = Arc::new(FakeProvider {
            base_url: "https://api.example.com".to_string(),
            api_key: None,
        });
        let wrapped = ProxiedProvider::wrap(real, "http://127.0.0.1:9999".to_string());

        use crate::registry::Named;
        assert_eq!(wrapped.name(), "fake");
        assert_eq!(wrapped.instance_id(), "so fake!");
        assert_eq!(
            wrapped.model_alias("granite".to_string(), None),
            None,
            "the alias must come from the real provider, so the route key registered \
             and the name the launched process sends stay the same string"
        );
    }
}
