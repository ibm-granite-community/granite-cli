use crate::capabilities::requirement::{
    ModelRequirement, ProviderRequirement, ShellCommandRequirement,
};
use crate::dependency::Configured;
use crate::models::Model;
use crate::providers::ApiType;
use crate::registry::{ConfigConstructable, Secret};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// Canonical launch-time types live in `launchers::base` -- re-exported here so
// capabilities and launchers share one `LaunchContext`/`EnvBinding` pair.
pub use crate::launchers::{EnvBinding, LaunchContext};

/*-- BindingType / BindingRequest / Binding -----------------------------------*/

/// Declares one binding surface a `Capability` can fill, together with the
/// request payload it takes and the result payload it produces. Expands into
/// matching variants of `BindingType` (payload-free, hashable), `BindingRequest`,
/// and `Binding` -- one macro invocation site, so a new binding surface can't
/// be added to one enum without the matching variant in the other two.
macro_rules! define_bindings {
    ($(
        $variant:ident {
            request: $request_ty:ty,
            result: $result_ty:ty,
            display: $display:literal,
        }
    ),+ $(,)?) => {
        /// Which binding surface a `Capability` can fill. Payload-free and
        /// hashable so a `Launcher` can declare `HashSet<BindingType>` for
        /// the surfaces it knows how to consume.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum BindingType {
            $($variant),+
        }

        impl std::fmt::Display for BindingType {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(BindingType::$variant => write!(f, $display),)+
                }
            }
        }

        /// A request for a capability to produce a `Binding` for a specific
        /// binding surface, parameterized by whatever detail that surface
        /// needs (e.g. which `ApiType` the launcher's environment expects).
        #[derive(Debug, Clone)]
        pub enum BindingRequest {
            $($variant($request_ty)),+
        }

        impl BindingRequest {
            pub fn binding_type(&self) -> BindingType {
                match self {
                    $(BindingRequest::$variant(_) => BindingType::$variant,)+
                }
            }
        }

        /// The result of a successful `Capability::bind` call.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum Binding {
            $($variant($result_ty)),+
        }

        impl Binding {
            pub fn binding_type(&self) -> BindingType {
                match self {
                    $(Binding::$variant(_) => BindingType::$variant,)+
                }
            }
        }
    };
}

/// Request payload for `BindingType::AgentModel` -- which `ApiType` the
/// launcher's environment expects.
#[derive(Debug, Clone)]
pub struct AgentModelBindingRequest {
    pub api_type: ApiType,
}

/// Result payload for `BindingType::AgentModel`: a configured model's
/// connection details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentModelBinding {
    pub api_type: ApiType,
    pub base_url: String,
    pub model_name: String,
    pub endpoint_path: String,
    pub api_key: Option<Secret>,
    pub verify_ssl: bool,
}

define_bindings! {
    AgentModel {
        request: AgentModelBindingRequest,
        result: AgentModelBinding,
        display: "Agent Model",
    },
}

/*-- Capability Trait ----------------------------------------------------------*/

/// Core trait for capability implementations.
/// All capabilities must implement this trait along with ConfigConstructable.
#[async_trait]
pub trait Capability: ConfigConstructable + Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn dependencies(&self) -> Vec<Dependency>;

    /// Which binding surfaces this capability instance can fill.
    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::new()
    }

    /// Resolve a `BindingRequest` into a concrete `Binding`, looking up this
    /// capability's model dependency from the given source. A model's own
    /// provider is reached via `Model::provider()`.
    async fn bind(
        &self,
        request: BindingRequest,
        models: &(dyn Configured<dyn Model> + Sync),
    ) -> anyhow::Result<Binding>;

    // Execution hooks (all optional with NoOp defaults)
    async fn on_setup(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_pre_launch(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_post_launch(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_shutdown(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn runtime_bindings(&self) -> Vec<EnvBinding> {
        vec![]
    }
}

/*-- Metadata Types ----------------------------------------------------------*/

/// Metadata describing a capability implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityMetadata {
    pub name: String,
    pub description: String,
    pub dependencies: Vec<Dependency>,
    pub tags: Vec<String>,
    /// Binding surfaces this capability *type* can support (superset); a
    /// concrete instance may choose to support only a subset via
    /// `Capability::binding_types`.
    pub supported_binding_types: HashSet<BindingType>,
}

impl std::fmt::Display for CapabilityMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description)
    }
}

/*-- Supporting Types --------------------------------------------------------*/

/// A capability's declared dependency on a model, provider, or external shell
/// command. `resolved_id` is `None` at the type level (catalog display,
/// before any instance is configured) and `Some(id)` once a concrete
/// instance has picked a specific dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Dependency {
    Model {
        /// The JSON key in this capability's own config that the resolved
        /// model id is stored under.
        config_key: String,
        requirement: ModelRequirement,
        resolved_id: Option<String>,
        required: bool,
    },
    Provider {
        /// The JSON key in this capability's own config that the resolved
        /// provider id is stored under.
        config_key: String,
        requirement: ProviderRequirement,
        resolved_id: Option<String>,
        required: bool,
    },
    ExternalTool {
        requirement: ShellCommandRequirement,
        required: bool,
    },
}

impl std::fmt::Display for Dependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dependency::Model {
                resolved_id,
                required,
                ..
            } => {
                write!(
                    f,
                    "Model: {}{}",
                    resolved_id.as_deref().unwrap_or("<unresolved>"),
                    if *required { " (required)" } else { "" }
                )
            }
            Dependency::Provider {
                resolved_id,
                required,
                ..
            } => {
                write!(
                    f,
                    "Provider: {}{}",
                    resolved_id.as_deref().unwrap_or("<unresolved>"),
                    if *required { " (required)" } else { "" }
                )
            }
            Dependency::ExternalTool {
                requirement,
                required,
            } => {
                write!(
                    f,
                    "ExternalTool: {}{}",
                    requirement.command,
                    if *required { " (required)" } else { "" }
                )
            }
        }
    }
}

/*-- Factory Definition ------------------------------------------------------*/

use crate::define_factory;

define_factory!(Capability, CapabilityMetadata, CapabilityFactory);
