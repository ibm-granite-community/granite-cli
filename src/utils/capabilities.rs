use std::collections::HashMap;

// Local
use crate::capabilities::Dependency;
use crate::config::CapabilityConfig;

/// Extract model slots from a capability's config, keyed by their
/// dependency `config_key`. For each `Dependency::Model`, looks up the
/// corresponding value in `cap_cfg.config` and returns the mapping.
///
/// Returns an empty map when the capability has no model dependencies.
pub(crate) fn capability_model_ids(
    cap_cfg: &CapabilityConfig,
    dependencies: &[Dependency],
) -> HashMap<String, String> {
    let mut model_ids = HashMap::new();
    for dep in dependencies {
        let Dependency::Model { config_key, .. } = dep else {
            continue;
        };
        if let Some(model_id) = cap_cfg
            .config
            .get(config_key.as_str())
            .and_then(|v| v.as_str())
        {
            model_ids.insert(config_key.clone(), model_id.to_string());
        }
    }
    model_ids
}
