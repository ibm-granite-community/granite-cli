//! Test doubles for the factory-constructed traits, shared by the tests that
//! build them.
//!
//! Each test module used to define its own: seven copies of one
//! `FakeProvider`, and six model doubles that differed in an instance id, a
//! `ModelType`, a repository string and whether they carried variants. One
//! definition of each lives here, so a change to `ConfigConstructable` or to
//! a domain trait edits one implementation.

// Standard
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

// Local
use crate::models::{Model, ModelArchitecture, ModelFunction, ModelType, ModelVariant};
use crate::providers::{
    ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError, PullResult,
};
use crate::registry::{ConfigConstructable, ConstructError, Named, Secret};
use crate::utils::install::{InstallEnv, Os, RunOutcome};
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

/// An install environment that answers from tables the test fills in and
/// records what it was asked to do. It runs no command, looks nothing up on
/// PATH and opens no connection.
///
/// A program is found when the test listed it, or when a scripted command
/// installed it. A command the test did not script fails. Health checks
/// answer from a queue and report unhealthy once it is empty.
pub(crate) struct FakeInstallEnv {
    os: Os,
    wsl: bool,
    path: Mutex<HashMap<String, PathBuf>>,
    dirs: Mutex<HashMap<(String, String), PathBuf>>,
    scripted: HashMap<String, FakeRun>,
    runs: Mutex<Vec<String>>,
    health: Mutex<VecDeque<bool>>,
    health_checks: Mutex<Vec<(String, serde_json::Value)>>,
}

/// Whether a scripted command succeeds, and what it puts on the machine.
#[derive(Clone, Default)]
pub(crate) struct FakeRun {
    success: bool,
    on_path: Vec<(String, PathBuf)>,
    in_dir: Vec<(String, String, PathBuf)>,
}

impl FakeRun {
    pub(crate) fn ok() -> Self {
        Self {
            success: true,
            ..Default::default()
        }
    }

    pub(crate) fn failed() -> Self {
        Self::default()
    }

    /// The command puts `program` on PATH at `path`.
    pub(crate) fn installs(mut self, program: &str, path: &str) -> Self {
        self.on_path
            .push((program.to_string(), PathBuf::from(path)));
        self
    }

    /// The command puts `program` at `path` in `dir`, which is not on PATH.
    pub(crate) fn installs_in(mut self, dir: &str, program: &str, path: &str) -> Self {
        self.in_dir
            .push((dir.to_string(), program.to_string(), PathBuf::from(path)));
        self
    }
}

impl Default for FakeInstallEnv {
    /// A Linux machine with nothing on PATH.
    fn default() -> Self {
        Self::new(Os::Linux)
    }
}

impl FakeInstallEnv {
    pub(crate) fn new(os: Os) -> Self {
        Self {
            os,
            wsl: false,
            path: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            scripted: HashMap::new(),
            runs: Mutex::new(Vec::new()),
            health: Mutex::new(VecDeque::new()),
            health_checks: Mutex::new(Vec::new()),
        }
    }

    /// Put each of `programs` on PATH at `/usr/bin/<program>`.
    pub(crate) fn with_programs(self, programs: &[&str]) -> Self {
        for p in programs {
            self.path
                .lock()
                .unwrap()
                .insert(p.to_string(), PathBuf::from(format!("/usr/bin/{p}")));
        }
        self
    }

    pub(crate) fn wsl(mut self) -> Self {
        self.wsl = true;
        self
    }

    pub(crate) fn with_run(mut self, command: &str, run: FakeRun) -> Self {
        self.scripted.insert(command.to_string(), run);
        self
    }

    /// Answers for the next health checks, in order.
    pub(crate) fn with_health(self, answers: &[bool]) -> Self {
        self.health.lock().unwrap().extend(answers);
        self
    }

    /// Every command run so far.
    pub(crate) fn runs(&self) -> Vec<String> {
        self.runs.lock().unwrap().clone()
    }

    /// The provider type of every health check so far.
    pub(crate) fn health_checks(&self) -> Vec<String> {
        self.health_checks
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl InstallEnv for FakeInstallEnv {
    fn os(&self) -> Os {
        self.os
    }

    fn is_wsl(&self) -> bool {
        self.wsl
    }

    fn find(&self, program: &str) -> Option<PathBuf> {
        self.path.lock().unwrap().get(program).cloned()
    }

    fn find_in(&self, dir: &str, program: &str) -> Option<PathBuf> {
        self.dirs
            .lock()
            .unwrap()
            .get(&(dir.to_string(), program.to_string()))
            .cloned()
    }

    async fn run(&self, command: &str) -> anyhow::Result<RunOutcome> {
        self.runs.lock().unwrap().push(command.to_string());
        let Some(run) = self.scripted.get(command) else {
            return Ok(RunOutcome {
                success: false,
                status: "exit status 127".to_string(),
            });
        };
        if run.success {
            let mut path = self.path.lock().unwrap();
            for (program, at) in &run.on_path {
                path.insert(program.clone(), at.clone());
            }
            let mut dirs = self.dirs.lock().unwrap();
            for (dir, program, at) in &run.in_dir {
                dirs.insert((dir.clone(), program.clone()), at.clone());
            }
        }
        Ok(RunOutcome {
            success: run.success,
            status: if run.success {
                "exit status 0".to_string()
            } else {
                "exit status 1".to_string()
            },
        })
    }

    async fn check_provider(
        &self,
        provider_type: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<HealthStatus> {
        self.health_checks
            .lock()
            .unwrap()
            .push((provider_type.to_string(), config.clone()));
        let healthy = self.health.lock().unwrap().pop_front().unwrap_or(false);
        Ok(HealthStatus {
            healthy,
            latency: std::time::Duration::ZERO,
            error: (!healthy).then(|| "Connection failed: no server in tests".to_string()),
        })
    }
}
