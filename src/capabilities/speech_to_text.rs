//! `SpeechToTextCapability`: surfaces a transcription-capable model's
//! connection details so a launcher can wire up its voice-input mode.

use crate::capabilities::base::{
    Binding, BindingRequest, BindingType, Capability, CapabilityMetadata, Dependency,
    HasCapabilityMetadata, VoiceInputBinding, VoiceInputBindingRequest,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::models::{ConfiguredModel, ModelFunction};
use crate::registry::{ConfigConstructable, ConstructError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_valid::Validate;
use std::collections::HashSet;

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, Validate)]
pub(crate) struct SpeechToTextCapabilityConfig {
    /// Key into the configured models map (the user-chosen instance ID) for
    /// the transcription model this capability serves.
    #[validate(min_length = 1, message = "no model is selected")]
    pub(crate) model_id: String,
    /// Language hint (e.g. `en`). Leave unset to auto-detect.
    #[serde(default)]
    pub(crate) language: Option<String>,
    /// Request timeout in seconds; raise for self-hosted model cold starts.
    #[serde(default = "default_timeout_seconds")]
    pub(crate) timeout_seconds: u64,
    /// Transport-level retries; 0 disables.
    #[serde(default = "default_max_retries")]
    pub(crate) max_retries: u32,
}

impl Default for SpeechToTextCapabilityConfig {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            language: None,
            timeout_seconds: default_timeout_seconds(),
            max_retries: default_max_retries(),
        }
    }
}

pub(crate) struct SpeechToTextCapability {
    instance_id: String,
    config: SpeechToTextCapabilityConfig,
}

/// [`SpeechToTextCapability`] with the model its `model_id` names. Built only
/// by `resolve_refs`.
pub(crate) struct ResolvedSpeechToTextCapability {
    inner: SpeechToTextCapability,
    configured_model: ConfiguredModel,
}

impl ConfigConstructable for SpeechToTextCapability {
    type Config = SpeechToTextCapabilityConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: SpeechToTextCapabilityConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        config
            .validate()
            .map_err(|e| crate::capabilities::base::invalid_settings(&e))?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
        })
    }
}

impl crate::registry::Named for SpeechToTextCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl crate::registry::Named for ResolvedSpeechToTextCapability {
    fn instance_id(&self) -> &str {
        self.inner.instance_id()
    }
}

impl crate::capabilities::CapabilityInfo for SpeechToTextCapability {
    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::VoiceInput])
    }
}

impl crate::capabilities::CapabilityInfo for ResolvedSpeechToTextCapability {
    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::VoiceInput])
    }
}

impl Capability for SpeechToTextCapability {
    fn resolve_refs(
        self: Box<Self>,
        models: &dyn crate::models::ModelLookup,
    ) -> anyhow::Result<Box<dyn crate::capabilities::ResolvedCapability>> {
        let configured_model = crate::capabilities::base::resolve_declared_model(
            models,
            &Self::metadata(),
            &self.config.model_id,
        )?;
        Ok(Box::new(ResolvedSpeechToTextCapability {
            inner: *self,
            configured_model,
        }))
    }
}

#[async_trait]
impl crate::capabilities::ResolvedCapability for ResolvedSpeechToTextCapability {
    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let VoiceInputBindingRequest { api_type } = match request {
            BindingRequest::VoiceInput(request) => request,
            other => anyhow::bail!(
                "SpeechToTextCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };
        let (provider, endpoint, model_name) = self.configured_model.resolve_provider_endpoint(
            &self.inner.config.model_id,
            api_type.clone(),
            ModelFunction::Transcription,
        )?;

        Ok(Binding::VoiceInput(VoiceInputBinding {
            api_type,
            provider_name: provider.instance_id().to_string(),
            base_url: provider.base_url().to_string(),
            model_name,
            endpoint_path: endpoint.path().to_string(),
            api_key: provider.api_key().cloned(),
            verify_ssl: provider.verify_ssl(),
            language: self.inner.config.language.clone().filter(|l| !l.is_empty()),
            timeout_seconds: self.inner.config.timeout_seconds,
            max_retries: self.inner.config.max_retries,
            custom_headers: provider.custom_headers(),
        }))
    }
}

impl HasCapabilityMetadata for SpeechToTextCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: NAME.to_string(),
            description: DESCRIPTION.to_string(),
            dependencies: vec![Dependency::Model {
                config_key: "model_id".to_string(),
                requirement: ModelRequirement {
                    supported_functions: vec![ModelFunction::Transcription],
                    ..Default::default()
                },
                resolved_id: None,
                required: true,
            }],
            tags: vec![
                "voice".to_string(),
                "audio".to_string(),
                "speech".to_string(),
            ],
            supported_binding_types: HashSet::from([BindingType::VoiceInput]),
            auto_configurable: true,
        }
    }
}

/*-- private --*/

const NAME: &str = "Speech To Text";
const DESCRIPTION: &str =
    "Surfaces a transcription model's connection details to a launched agent's voice input mode.";

fn default_timeout_seconds() -> u64 {
    60
}

fn default_max_retries() -> u32 {
    1
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{CapabilityInfo, ResolvedCapability};
    use crate::providers::{ApiEndpoint, ApiType};
    use crate::utils::test_support::{FakeModel, FakeProvider};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn resolved(cfg: serde_json::Value, with_endpoint: bool) -> ResolvedSpeechToTextCapability {
        let endpoints = if with_endpoint {
            HashMap::from([(
                ModelFunction::Transcription,
                vec![ApiEndpoint::OpenAIAudioTranscription],
            )])
        } else {
            HashMap::new()
        };
        let provider = FakeProvider {
            instance_id: "my-llama".to_string(),
            base_url: "http://localhost:9931".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: vec![ApiType::OpenAI],
            endpoints,
            alias: None,
        };
        ResolvedSpeechToTextCapability {
            inner: SpeechToTextCapability::new("stt", &cfg).unwrap(),
            configured_model: ConfiguredModel::for_test(
                Arc::new(FakeModel::text(vec![ModelFunction::Transcription])),
                Arc::new(provider),
                None,
            ),
        }
    }

    fn request() -> BindingRequest {
        BindingRequest::VoiceInput(VoiceInputBindingRequest {
            api_type: ApiType::OpenAI,
        })
    }

    #[tokio::test]
    async fn bind_resolves_transcription_endpoint_and_defaults() {
        let cap = resolved(serde_json::json!({ "model_id": "m" }), true);
        let Binding::VoiceInput(b) = cap.bind(request()).await.unwrap() else {
            panic!("expected VoiceInput binding")
        };
        assert_eq!(b.base_url, "http://localhost:9931");
        assert_eq!(b.endpoint_path, "/v1/audio/transcriptions");
        assert_eq!(b.language, None);
        assert_eq!(b.timeout_seconds, 60);
        assert_eq!(b.max_retries, 1);
    }

    #[tokio::test]
    async fn bind_passes_language_and_treats_empty_as_auto_detect() {
        let cap = resolved(
            serde_json::json!({ "model_id": "m", "language": "es" }),
            true,
        );
        let Binding::VoiceInput(b) = cap.bind(request()).await.unwrap() else {
            panic!()
        };
        assert_eq!(b.language.as_deref(), Some("es"));

        let cap = resolved(serde_json::json!({ "model_id": "m", "language": "" }), true);
        let Binding::VoiceInput(b) = cap.bind(request()).await.unwrap() else {
            panic!()
        };
        assert_eq!(b.language, None);
    }

    #[tokio::test]
    async fn bind_fails_without_a_transcription_endpoint() {
        let cap = resolved(serde_json::json!({ "model_id": "m" }), false);
        let err = cap.bind(request()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("no OpenAI endpoint for Transcription")
        );
    }

    #[tokio::test]
    async fn bind_rejects_other_binding_requests() {
        let cap = resolved(serde_json::json!({ "model_id": "m" }), true);
        let err = cap
            .bind(BindingRequest::AgentModel(
                crate::capabilities::AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                },
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not handle"));
    }

    #[test]
    fn empty_model_id_is_rejected() {
        assert!(
            SpeechToTextCapability::new("stt", &serde_json::json!({ "model_id": "" })).is_err()
        );
    }

    #[test]
    fn metadata_requires_transcription_model_and_voice_input_binding() {
        let meta = SpeechToTextCapability::metadata();
        assert_eq!(
            meta.supported_binding_types,
            HashSet::from([BindingType::VoiceInput])
        );
        assert!(meta.dependencies.iter().any(|d| matches!(
            d,
            Dependency::Model { requirement, required: true, .. }
                if requirement.supported_functions == vec![ModelFunction::Transcription]
        )));
        let cap =
            SpeechToTextCapability::new("stt", &serde_json::json!({ "model_id": "m" })).unwrap();
        assert_eq!(
            cap.binding_types(),
            HashSet::from([BindingType::VoiceInput])
        );
    }
}
