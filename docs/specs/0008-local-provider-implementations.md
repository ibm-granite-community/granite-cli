# Spec 0008: Local Provider Implementations (Ollama, llama.cpp, LM Studio)

## Overview

This specification details the implementation of three local inference providers: Ollama, llama.cpp, and LM Studio. These providers enable running models locally with different API compatibility layers and model format support.

## Goals

1. Add support for Ollama provider with configurable endpoint (default: http://localhost:11434)
2. Add support for llama.cpp provider with configurable endpoint (default: http://localhost:8080)
3. Add support for LM Studio provider with configurable endpoint (default: http://localhost:1234)
4. Add MLX format support for Apple Silicon compatibility
5. Ensure proper API endpoint mapping for each provider
6. Support platform-specific features (MLX on macOS)
7. Maximize code reuse through shared helper functions
8. Fix provider selection logic to gate on format only, not precision

## Non-Goals

- Implementing model downloading or management
- Adding new API types beyond OpenAI, Ollama, and Anthropic
- Implementing provider-specific optimizations
- Changing the existing OpenAIProvider implementation

## Design Decisions

### Code Reuse Strategy

To avoid duplicating health check logic across providers, we will:
1. Create a shared `http_health_check` helper function in `src/providers/base.rs`
2. Each provider calls this helper from their `health_check` implementation
3. All providers use the same configuration structure for consistency

### Configuration Consistency

All providers will use the same configuration structure (including optional `api_key`) even though local providers typically don't require authentication. This provides:
- Consistent user experience
- Future flexibility if authentication is added
- Simplified configuration schema generation

### Provider Selection Logic Fix

**Current Issue**: In `commands/model.rs`, the `VariantRequirement::admits_type` method checks both `supported_formats` AND `supported_precisions`. This is overly restrictive because:
- It filters out providers that could handle the format but don't list a specific precision
- Precision compatibility should be determined at runtime by `can_run_model`
- Provider metadata's `supported_precisions` should be informational, not a hard gate

**Solution**: Modify `admits_type` to only check format compatibility:

```rust
// In src/commands/model.rs
impl Requirement<dyn Provider> for VariantRequirement {
    fn admits_type(&self, metadata: &ProviderMetadata) -> bool {
        // Only gate on format - precision is checked by can_run_model
        metadata
            .supported_formats
            .iter()
            .any(|f| f.to_string().eq_ignore_ascii_case(&self.format))
    }

    fn admits_instance(&self, instance: &dyn Provider) -> bool {
        // This is where precision compatibility is actually determined
        instance.can_run_model(&self.format, &self.precision)
    }
}
```

This change means:
- `supported_precisions` in metadata is informational (documents what the provider typically supports)
- Actual precision acceptance is determined by each provider's `can_run_model` implementation
- Providers have flexibility to accept precisions not explicitly listed in metadata

## Implementation Details

### 1. Shared Health Check Helper

Add to `src/providers/base.rs`:

```rust
use std::time::Instant;

/// Shared HTTP health check implementation for providers
pub async fn http_health_check(
    client: &reqwest::Client,
    base_url: &str,
    health_endpoint: &str,
    api_key: Option<&Secret>,
) -> Result<HealthStatus, ProviderError> {
    let start = Instant::now();
    let url = format!("{}{}", base_url, health_endpoint);
    
    let mut request = client.get(&url);
    
    if let Some(key) = api_key {
        request = request.bearer_auth(&key.0);
    }
    
    match request.send().await {
        Ok(response) => {
            let latency = start.elapsed();
            
            if response.status().is_success() {
                Ok(HealthStatus {
                    healthy: true,
                    latency,
                    error: None,
                })
            } else {
                Ok(HealthStatus {
                    healthy: false,
                    latency,
                    error: Some(format!("HTTP {}: {}",
                        response.status(),
                        response.text().await.unwrap_or_default()
                    )),
                })
            }
        }
        Err(e) => {
            let latency = start.elapsed();
            Ok(HealthStatus {
                healthy: false,
                latency,
                error: Some(format!("Connection failed: {}", e)),
            })
        }
    }
}
```

### 2. MLX Format Addition

Add to `ModelFormat` enum in `src/providers/base.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum ModelFormat {
    Safetensors,
    GGUF,
    ONNX,
    MLX,  // Apple Silicon optimized format
}
```

Update Display implementation:
```rust
impl std::fmt::Display for ModelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelFormat::Safetensors => write!(f, "safetensors"),
            ModelFormat::GGUF => write!(f, "GGUF"),
            ModelFormat::ONNX => write!(f, "ONNX"),
            ModelFormat::MLX => write!(f, "MLX"),
        }
    }
}
```

### 3. Ollama Provider (`src/providers/ollama.rs`)

#### Configuration

```rust
use crate::registry::Secret;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OllamaProviderConfig {
    /// Base URL for the Ollama API
    #[serde(default = "default_ollama_url")]
    pub base_url: String,

    /// API key for authentication (optional)
    pub api_key: Option<Secret>,

    /// Timeout for health checks in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,

    /// Endpoint to use for health checks
    #[serde(default = "default_ollama_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_ollama_health_endpoint() -> String {
    "/api/tags".to_string()
}

impl Default for OllamaProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_ollama_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_ollama_health_endpoint(),
        }
    }
}
```

#### Provider Implementation

```rust
use crate::models::ModelFunction;
use crate::providers::base::{
    http_health_check, ApiEndpoint, ApiType, AuthType, HealthStatus, 
    ModelFormat, Provider, ProviderError, ProviderMetadata, ProviderType,
    HasProviderMetadata,
};
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

pub struct OllamaProvider {
    config: OllamaProviderConfig,
    client: reqwest::Client,
}

impl OllamaProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();
        
        // Chat functions can use OpenAI, Ollama, or Anthropic endpoints
        map.insert(ModelFunction::Chat, vec![
            ApiEndpoint::OpenAIChat,
            ApiEndpoint::OllamaChat,
            ApiEndpoint::AnthropicMessages,
        ]);
        
        // Embeddings can use OpenAI or Ollama endpoints
        map.insert(ModelFunction::Embeddings, vec![
            ApiEndpoint::OpenAIEmbeddings,
            ApiEndpoint::OllamaEmbeddings,
        ]);
        
        map
    }
}

impl ConfigConstructable for OllamaProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: OllamaProviderConfig = serde_json::from_value(cfg.clone())
            .unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "Ollama"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        Self::default_function_endpoints()
    }

    fn supported_api_types(&self) -> Vec<ApiType> {
        vec![ApiType::OpenAI, ApiType::Ollama, ApiType::Anthropic]
    }

    fn supported_formats(&self) -> Vec<ModelFormat> {
        vec![ModelFormat::GGUF]
    }

    fn supported_precisions(&self) -> Vec<String> {
        vec![
            "Q2_K".to_string(),
            "Q3_K_S".to_string(),
            "Q3_K_M".to_string(),
            "Q3_K_L".to_string(),
            "Q4_0".to_string(),
            "Q4_K_S".to_string(),
            "Q4_K_M".to_string(),
            "Q5_0".to_string(),
            "Q5_K_S".to_string(),
            "Q5_K_M".to_string(),
            "Q6_K".to_string(),
            "Q8_0".to_string(),
            "fp16".to_string(),
            "fp32".to_string(),
        ]
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        // Ollama can run any GGUF model regardless of precision
        variant_format.eq_ignore_ascii_case("gguf")
    }

    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        http_health_check(
            &self.client,
            &self.config.base_url,
            &self.config.health_check_endpoint,
            self.config.api_key.as_ref(),
        ).await
    }
}

impl HasProviderMetadata for OllamaProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata {
            name: "Ollama".to_string(),
            description: "Local inference server supporting multiple API protocols and GGUF models".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:11434".to_string(),
            supported_api_types: vec![ApiType::OpenAI, ApiType::Ollama, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: vec![ModelFormat::GGUF],
            supported_precisions: vec![
                "Q2_K".to_string(),
                "Q3_K_S".to_string(),
                "Q3_K_M".to_string(),
                "Q3_K_L".to_string(),
                "Q4_0".to_string(),
                "Q4_K_S".to_string(),
                "Q4_K_M".to_string(),
                "Q5_0".to_string(),
                "Q5_K_S".to_string(),
                "Q5_K_M".to_string(),
                "Q6_K".to_string(),
                "Q8_0".to_string(),
                "fp16".to_string(),
                "fp32".to_string(),
            ],
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags: vec![
                "ollama".to_string(),
                "local".to_string(),
                "gguf".to_string(),
                "multi-api".to_string(),
            ],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(OllamaProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(OllamaProviderConfig::default()).unwrap_or_default()
    }
}
```

### 4. llama.cpp Provider (`src/providers/llamacpp.rs`)

#### Configuration

```rust
use crate::registry::Secret;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LlamaCppProviderConfig {
    /// Base URL for the llama.cpp server
    #[serde(default = "default_llamacpp_url")]
    pub base_url: String,

    /// API key for authentication (optional)
    pub api_key: Option<Secret>,

    /// Timeout for health checks in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,

    /// Endpoint to use for health checks
    #[serde(default = "default_llamacpp_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_llamacpp_url() -> String {
    "http://localhost:8080".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_llamacpp_health_endpoint() -> String {
    "/health".to_string()
}

impl Default for LlamaCppProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_llamacpp_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_llamacpp_health_endpoint(),
        }
    }
}
```

#### Provider Implementation

```rust
use crate::models::ModelFunction;
use crate::providers::base::{
    http_health_check, ApiEndpoint, ApiType, AuthType, HealthStatus,
    ModelFormat, Provider, ProviderError, ProviderMetadata, ProviderType,
    HasProviderMetadata,
};
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

pub struct LlamaCppProvider {
    config: LlamaCppProviderConfig,
    client: reqwest::Client,
}

impl LlamaCppProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();
        
        // Chat functions can use OpenAI or Anthropic endpoints
        map.insert(ModelFunction::Chat, vec![
            ApiEndpoint::OpenAIChat,
            ApiEndpoint::AnthropicMessages,
        ]);
        
        // Embeddings use OpenAI endpoint
        map.insert(ModelFunction::Embeddings, vec![
            ApiEndpoint::OpenAIEmbeddings,
        ]);
        
        map
    }
}

impl ConfigConstructable for LlamaCppProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: LlamaCppProviderConfig = serde_json::from_value(cfg.clone())
            .unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }
}

#[async_trait]
impl Provider for LlamaCppProvider {
    fn name(&self) -> &str {
        "llama.cpp"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        Self::default_function_endpoints()
    }

    fn supported_api_types(&self) -> Vec<ApiType> {
        vec![ApiType::OpenAI, ApiType::Anthropic]
    }

    fn supported_formats(&self) -> Vec<ModelFormat> {
        vec![ModelFormat::GGUF]
    }

    fn supported_precisions(&self) -> Vec<String> {
        vec![
            "Q2_K".to_string(),
            "Q3_K_S".to_string(),
            "Q3_K_M".to_string(),
            "Q3_K_L".to_string(),
            "Q4_0".to_string(),
            "Q4_K_S".to_string(),
            "Q4_K_M".to_string(),
            "Q5_0".to_string(),
            "Q5_K_S".to_string(),
            "Q5_K_M".to_string(),
            "Q6_K".to_string(),
            "Q8_0".to_string(),
            "fp16".to_string(),
        ]
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        // llama.cpp can run any GGUF model regardless of precision
        variant_format.eq_ignore_ascii_case("gguf")
    }

    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        http_health_check(
            &self.client,
            &self.config.base_url,
            &self.config.health_check_endpoint,
            self.config.api_key.as_ref(),
        ).await
    }
}

impl HasProviderMetadata for LlamaCppProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata {
            name: "llama.cpp".to_string(),
            description: "High-performance local inference server for GGUF models with OpenAI and Anthropic API compatibility".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:8080".to_string(),
            supported_api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: vec![ModelFormat::GGUF],
            supported_precisions: vec![
                "Q2_K".to_string(),
                "Q3_K_S".to_string(),
                "Q3_K_M".to_string(),
                "Q3_K_L".to_string(),
                "Q4_0".to_string(),
                "Q4_K_S".to_string(),
                "Q4_K_M".to_string(),
                "Q5_0".to_string(),
                "Q5_K_S".to_string(),
                "Q5_K_M".to_string(),
                "Q6_K".to_string(),
                "Q8_0".to_string(),
                "fp16".to_string(),
            ],
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags: vec![
                "llama.cpp".to_string(),
                "local".to_string(),
                "gguf".to_string(),
                "high-performance".to_string(),
            ],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(LlamaCppProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(LlamaCppProviderConfig::default()).unwrap_or_default()
    }
}
```

### 5. LM Studio Provider (`src/providers/lmstudio.rs`)

#### Configuration

```rust
use crate::registry::Secret;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LMStudioProviderConfig {
    /// Base URL for the LM Studio server
    #[serde(default = "default_lmstudio_url")]
    pub base_url: String,

    /// API key for authentication (optional)
    pub api_key: Option<Secret>,

    /// Timeout for health checks in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,

    /// Endpoint to use for health checks
    #[serde(default = "default_lmstudio_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_lmstudio_url() -> String {
    "http://localhost:1234".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_lmstudio_health_endpoint() -> String {
    "/v1/models".to_string()
}

impl Default for LMStudioProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_lmstudio_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_lmstudio_health_endpoint(),
        }
    }
}
```

#### Provider Implementation

```rust
use crate::models::ModelFunction;
use crate::providers::base::{
    http_health_check, ApiEndpoint, ApiType, AuthType, HealthStatus,
    ModelFormat, Provider, ProviderError, ProviderMetadata, ProviderType,
    HasProviderMetadata,
};
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

pub struct LMStudioProvider {
    config: LMStudioProviderConfig,
    client: reqwest::Client,
}

impl LMStudioProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();
        
        // Chat functions can use OpenAI or Anthropic endpoints
        map.insert(ModelFunction::Chat, vec![
            ApiEndpoint::OpenAIChat,
            ApiEndpoint::AnthropicMessages,
        ]);
        
        // Embeddings use OpenAI endpoint
        map.insert(ModelFunction::Embeddings, vec![
            ApiEndpoint::OpenAIEmbeddings,
        ]);
        
        map
    }
}

impl ConfigConstructable for LMStudioProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: LMStudioProviderConfig = serde_json::from_value(cfg.clone())
            .unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }
}

#[async_trait]
impl Provider for LMStudioProvider {
    fn name(&self) -> &str {
        "LM Studio"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        Self::default_function_endpoints()
    }

    fn supported_api_types(&self) -> Vec<ApiType> {
        vec![ApiType::OpenAI, ApiType::Anthropic]
    }

    fn supported_formats(&self) -> Vec<ModelFormat> {
        let mut formats = vec![ModelFormat::GGUF];
        
        // Add MLX support on macOS
        if cfg!(target_os = "macos") {
            formats.push(ModelFormat::MLX);
        }
        
        formats
    }

    fn supported_precisions(&self) -> Vec<String> {
        vec![
            "Q2_K".to_string(),
            "Q3_K_S".to_string(),
            "Q3_K_M".to_string(),
            "Q3_K_L".to_string(),
            "Q4_0".to_string(),
            "Q4_K_S".to_string(),
            "Q4_K_M".to_string(),
            "Q5_0".to_string(),
            "Q5_K_S".to_string(),
            "Q5_K_M".to_string(),
            "Q6_K".to_string(),
            "Q8_0".to_string(),
            "fp16".to_string(),
            "fp32".to_string(),
        ]
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        match variant_format.to_lowercase().as_str() {
            "gguf" => true,
            "mlx" => cfg!(target_os = "macos"),
            _ => false,
        }
    }

    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        http_health_check(
            &self.client,
            &self.config.base_url,
            &self.config.health_check_endpoint,
            self.config.api_key.as_ref(),
        ).await
    }
}

impl HasProviderMetadata for LMStudioProvider {
    fn metadata() -> ProviderMetadata {
        let mut formats = vec![ModelFormat::GGUF];
        let mut tags = vec![
            "lm-studio".to_string(),
            "local".to_string(),
            "gguf".to_string(),
        ];
        
        // Add MLX support on macOS
        if cfg!(target_os = "macos") {
            formats.push(ModelFormat::MLX);
            tags.push("mlx".to_string());
            tags.push("apple-silicon".to_string());
        }
        
        ProviderMetadata {
            name: "LM Studio".to_string(),
            description: "User-friendly local inference server with GUI, supporting GGUF and MLX models".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:1234".to_string(),
            supported_api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: formats,
            supported_precisions: vec![
                "Q2_K".to_string(),
                "Q3_K_S".to_string(),
                "Q3_K_M".to_string(),
                "Q3_K_L".to_string(),
                "Q4_0".to_string(),
                "Q4_K_S".to_string(),
                "Q4_K_M".to_string(),
                "Q5_0".to_string(),
                "Q5_K_S".to_string(),
                "Q5_K_M".to_string(),
                "Q6_K".to_string(),
                "Q8_0".to_string(),
                "fp16".to_string(),
                "fp32".to_string(),
            ],
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags,
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(LMStudioProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(LMStudioProviderConfig::default()).unwrap_or_default()
    }
}
```

### 6. Provider Module Updates (`src/providers/mod.rs`)

Update the module to export new providers and register them:

```rust
pub static PROVIDER_REGISTRY: LazyLock<base::ProviderFactory> = LazyLock::new(|| {
    let mut factory = base::ProviderFactory::new();
    factory.register::<openai::OpenAIProvider>("openai-compatible");
    factory.register::<ollama::OllamaProvider>("ollama");
    factory.register::<llamacpp::LlamaCppProvider>("llama-cpp");
    factory.register::<lmstudio::LMStudioProvider>("lm-studio");
    factory
});

// Add module declarations
mod ollama;
pub use ollama::{OllamaProvider, OllamaProviderConfig};

mod llamacpp;
pub use llamacpp::{LlamaCppProvider, LlamaCppProviderConfig};

mod lmstudio;
pub use lmstudio::{LMStudioProvider, LMStudioProviderConfig};
```

### 7. Fix Provider Selection Logic (`src/commands/model.rs`)

Update the `VariantRequirement::admits_type` method to only check format:

```rust
impl Requirement<dyn Provider> for VariantRequirement {
    fn admits_type(&self, metadata: &ProviderMetadata) -> bool {
        // Only gate on format - precision is checked by can_run_model
        metadata
            .supported_formats
            .iter()
            .any(|f| f.to_string().eq_ignore_ascii_case(&self.format))
    }

    fn admits_instance(&self, instance: &dyn Provider) -> bool {
        // This is where precision compatibility is actually determined
        instance.can_run_model(&self.format, &self.precision)
    }
}
```

Update the test to reflect this change:

```rust
#[test]
fn admits_type_only_checks_format_not_precision() {
    let requirement = VariantRequirement { 
        format: "gguf".to_string(), 
        precision: "some-exotic-precision".to_string() 
    };
    let metadata = metadata_supporting(vec![ModelFormat::GGUF], vec!["fp16"]);
    // Should pass because format matches, even though precision doesn't
    assert!(requirement.admits_type(&metadata));
}

#[test]
fn admits_type_rejects_unsupported_format() {
    let requirement = VariantRequirement { 
        format: "gguf".to_string(), 
        precision: "fp16".to_string() 
    };
    let metadata = metadata_supporting(vec![ModelFormat::Safetensors], vec!["fp16"]);
    // Should fail because format doesn't match
    assert!(!requirement.admits_type(&metadata));
}
```

Remove the old test that checked precision in `admits_type`:

```rust
// DELETE THIS TEST - precision is no longer checked in admits_type
// #[test]
// fn admits_type_rejects_unsupported_precision() { ... }
```

## Implementation Plan

### Phase 1: Foundation
1. Add `http_health_check` helper function to `src/providers/base.rs`
2. Add MLX to `ModelFormat` enum
3. Update Display implementation for ModelFormat
4. Run tests to ensure no regressions

### Phase 2: Fix Provider Selection Logic
1. Update `VariantRequirement::admits_type` in `src/commands/model.rs` to only check format
2. Update related tests
3. Remove obsolete test for precision checking in `admits_type`
4. Run tests to verify the fix

### Phase 3: Ollama Provider
1. Create `src/providers/ollama.rs`
2. Implement `OllamaProviderConfig` with all fields
3. Implement `OllamaProvider` struct and `ConfigConstructable`
4. Implement `Provider` trait (using `http_health_check` helper)
5. Implement `HasProviderMetadata` trait
6. Add comprehensive unit tests

### Phase 4: llama.cpp Provider
1. Create `src/providers/llamacpp.rs`
2. Implement `LlamaCppProviderConfig` with all fields
3. Implement `LlamaCppProvider` struct and `ConfigConstructable`
4. Implement `Provider` trait (using `http_health_check` helper)
5. Implement `HasProviderMetadata` trait
6. Add comprehensive unit tests

### Phase 5: LM Studio Provider
1. Create `src/providers/lmstudio.rs`
2. Implement `LMStudioProviderConfig` with all fields
3. Implement `LMStudioProvider` struct and `ConfigConstructable`
4. Implement `Provider` trait with platform-specific format detection
5. Implement `HasProviderMetadata` trait with platform-specific metadata
6. Add comprehensive unit tests including platform-specific tests

### Phase 6: Integration
1. Update `src/providers/mod.rs` to export new providers
2. Register all providers in `PROVIDER_REGISTRY`
3. Run full test suite
4. Verify provider discovery and configuration

## Testing Strategy

### Unit Tests (per provider)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = XxxProviderConfig::default();
        assert_eq!(config.base_url, "http://localhost:XXXX");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
    }

    #[test]
    fn test_provider_metadata() {
        let meta = XxxProvider::metadata();
        assert_eq!(meta.name, "Xxx");
        assert!(meta.supported_api_types.contains(&ApiType::OpenAI));
        assert!(meta.default_function_endpoints.contains_key(&ModelFunction::Chat));
    }

    #[test]
    fn test_provider_constructs_from_json() {
        let cfg = serde_json::json!({
            "base_url": "http://example.com:8080",
            "timeout_secs": 30
        });
        let provider = XxxProvider::new(&cfg);
        assert_eq!(provider.config.base_url, "http://example.com:8080");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_gguf() {
        let provider = XxxProvider::new(&serde_json::json!({}));
        assert!(provider.can_run_model("gguf", "Q4_K_M"));
        assert!(provider.can_run_model("GGUF", "fp16"));
    }
}
```

### Platform-Specific Tests (LM Studio)

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_mlx_support_on_macos() {
        let provider = LMStudioProvider::new(&serde_json::json!({}));
        let formats = provider.supported_formats();
        
        #[cfg(target_os = "macos")]
        assert!(formats.contains(&ModelFormat::MLX));
        
        #[cfg(not(target_os = "macos"))]
        assert!(!formats.contains(&ModelFormat::MLX));
    }

    #[test]
    fn test_can_run_mlx_model() {
        let provider = LMStudioProvider::new(&serde_json::json!({}));
        
        #[cfg(target_os = "macos")]
        assert!(provider.can_run_model("mlx", "fp16"));
        
        #[cfg(not(target_os = "macos"))]
        assert!(!provider.can_run_model("mlx", "fp16"));
    }
}
```

### Integration Tests

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_all_providers_registered() {
        let catalog = PROVIDER_REGISTRY.entries();
        assert!(catalog.contains_key("openai-compatible"));
        assert!(catalog.contains_key("ollama"));
        assert!(catalog.contains_key("llama-cpp"));
        assert!(catalog.contains_key("lm-studio"));
    }

    #[test]
    fn test_provider_construction() {
        let providers = vec!["ollama", "llama-cpp", "lm-studio"];
        for provider_type in providers {
            let config = serde_json::json!({});
            let result = PROVIDER_REGISTRY.construct(provider_type, &config);
            assert!(result.is_ok(), "Failed to construct {}", provider_type);
        }
    }
}
```

### Updated Model Command Tests

```rust
#[test]
fn admits_type_only_checks_format_not_precision() {
    let requirement = VariantRequirement { 
        format: "gguf".to_string(), 
        precision: "some-exotic-precision".to_string() 
    };
    let metadata = metadata_supporting(vec![ModelFormat::GGUF], vec!["fp16"]);
    // Should pass because format matches, even though precision doesn't
    assert!(requirement.admits_type(&metadata));
}

#[test]
fn admits_instance_checks_precision_via_can_run_model() {
    let requirement = VariantRequirement { 
        format: "gguf".to_string(), 
        precision: "Q4_K_M".to_string() 
    };
    let provider = OllamaProvider::new(&serde_json::json!({}));
    // Should pass because Ollama's can_run_model accepts any GGUF precision
    assert!(requirement.admits_instance(&provider));
}
```

## Success Criteria

1. ✅ `http_health_check` helper function implemented and tested
2. ✅ MLX format added to `ModelFormat` enum
3. ✅ `VariantRequirement::admits_type` only checks format (not precision)
4. ✅ Ollama provider fully implemented with all three API types
5. ✅ llama.cpp provider fully implemented with OpenAI and Anthropic APIs
6. ✅ LM Studio provider fully implemented with platform-specific MLX support
7. ✅ All providers registered in `PROVIDER_REGISTRY`
8. ✅ All unit tests passing
9. ✅ All integration tests passing
10. ✅ Provider metadata correctly reflects capabilities
11. ✅ Configuration schemas generated correctly
12. ✅ No code duplication in health check logic
13. ✅ Provider selection logic correctly gates on format only

## Future Considerations

1. **Model Catalog Updates**: Update `resources/models.yaml` to include MLX variants for supported models
2. **Provider Health Monitoring**: Add periodic health checks for local providers
3. **Auto-discovery**: Auto-detect running local providers on standard ports
4. **Provider Preferences**: Allow users to specify preferred API type per provider
5. **Performance Metrics**: Track and display inference performance per provider
6. **Custom Health Endpoints**: Allow per-provider custom health check logic if needed
7. **Precision Validation**: Consider adding warnings when a provider accepts a precision not in its metadata list

## References

- [LM Studio OpenAI Compatibility](https://lmstudio.ai/docs/developer/openai-compat)
- [LM Studio Anthropic Compatibility](https://lmstudio.ai/docs/developer/anthropic-compat)
- [Ollama API Documentation](https://github.com/ollama/ollama/blob/main/docs/api.md)
- [llama.cpp Server Documentation](https://github.com/ggerganov/llama.cpp/tree/master/examples/server)
