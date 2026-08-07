use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=resources/models.yaml");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("generated_models.rs");

    let yaml_content =
        fs::read_to_string("resources/models.yaml").expect("Failed to read models.yaml");

    let models: Vec<YamlModel> =
        serde_yaml::from_str(&yaml_content).expect("Failed to parse models.yaml");

    let code = generate_models_code(&models);

    fs::write(&dest_path, code).expect("Failed to write generated code");

    // Generate version module to OUT_DIR (same pattern as generated_models.rs)
    let version_info = get_version_info();
    let version_code = generate_version_module(&version_info);
    let version_path = Path::new(&out_dir).join("version.rs");
    fs::write(&version_path, version_code).expect("Failed to write version.rs");
}

fn generate_models_code(models: &[YamlModel]) -> String {
    let mut code = String::from("// Auto-generated from resources/models.yaml - do not edit\n\n");

    // Generate a struct for each model
    for model in models {
        code.push_str(&generate_model_struct(model));
    }

    // Generate registration function
    code.push_str(
        "pub fn register_all_models(factory: &mut crate::models::base::ModelFactory) {\n",
    );
    for model in models {
        let struct_name = model_id_to_struct_name(&model.id);
        code.push_str(&format!(
            "    factory.register::<{}>(\"{}\");\n",
            struct_name, model.id
        ));
    }
    code.push_str("}\n");

    code
}

fn generate_model_struct(model: &YamlModel) -> String {
    let struct_name = model_id_to_struct_name(&model.id);
    let mut s = String::new();

    // Struct carrying the resolved provider config it was constructed with
    // (see `ModelSource::from_config`), if any.
    s.push_str(&format!(
        "pub struct {struct_name} {{ provider_config: Option<crate::config::ProviderConfig> }}\n\n"
    ));

    // ConfigConstructable implementation
    s.push_str(&format!(
        "impl crate::registry::ConfigConstructable for {struct_name} {{\n"
    ));
    s.push_str("    fn new(cfg: &serde_json::Value) -> Self {\n");
    s.push_str("        let provider_config = cfg.get(\"provider_config\").and_then(|v| serde_json::from_value(v.clone()).ok());\n");
    s.push_str("        Self { provider_config }\n");
    s.push_str("    }\n");
    s.push_str("}\n\n");

    // Model trait implementation
    s.push_str(&format!(
        "impl crate::models::base::Model for {struct_name} {{\n"
    ));
    s.push_str(&format!(
        "    fn family(&self) -> &str {{ {:?} }}\n",
        model.family
    ));
    s.push_str(&format!(
        "    fn version(&self) -> &str {{ {:?} }}\n",
        model.version
    ));
    s.push_str(&format!("    fn size(&self) -> u64 {{ {} }}\n", model.size));
    s.push_str(&format!(
        "    fn context_length(&self) -> u64 {{ {} }}\n",
        model.context_length
    ));
    s.push_str(&format!("    fn model_type(&self) -> &crate::models::base::ModelType {{ &crate::models::base::ModelType::{} }}\n", model.model_type));
    s.push_str(&format!(
        "    fn huggingface_repo(&self) -> &str {{ {:?} }}\n",
        model.huggingface_repo
    ));
    s.push_str(&format!(
        "    fn native_dtype(&self) -> &str {{ {:?} }}\n",
        model.native_dtype
    ));

    // Architecture - use static slice (contains a Vec, so not const-constructible)
    s.push_str("    fn architecture(&self) -> &crate::models::base::ModelArchitecture {\n");
    s.push_str("        static ARCHITECTURE: std::sync::LazyLock<crate::models::base::ModelArchitecture> = std::sync::LazyLock::new(|| ");
    s.push_str(&generate_architecture_literal(&model.architecture));
    s.push_str(");\n");
    s.push_str("        &ARCHITECTURE\n");
    s.push_str("    }\n");

    // Variants - use static slice
    s.push_str("    fn variants(&self) -> &[crate::models::base::ModelVariant] {\n");
    s.push_str("        static VARIANTS: std::sync::LazyLock<Vec<crate::models::base::ModelVariant>> = std::sync::LazyLock::new(|| vec![\n");
    for variant in &model.variants {
        s.push_str("            crate::models::base::ModelVariant {\n");
        s.push_str(&format!(
            "                format: {:?}.to_string(),\n",
            variant.format
        ));
        s.push_str(&format!(
            "                precision: {:?}.to_string(),\n",
            variant.precision
        ));
        s.push_str(&format!(
            "                size_gb: {},\n",
            format_float(variant.size_gb)
        ));
        s.push_str(&format!(
            "                url: {:?}.to_string(),\n",
            variant.url
        ));
        s.push_str("            },\n");
    }
    s.push_str("        ]);\n");
    s.push_str("        &VARIANTS\n");
    s.push_str("    }\n");

    // Description
    s.push_str("    fn description(&self) -> Option<&str> {\n");
    if let Some(ref desc) = model.description {
        s.push_str(&format!("        Some({desc:?})\n"));
    } else {
        s.push_str("        None\n");
    }
    s.push_str("    }\n");

    // Tags - use static slice
    s.push_str("    fn tags(&self) -> &[String] {\n");
    s.push_str("        static TAGS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| vec![\n");
    for tag in &model.tags {
        s.push_str(&format!("            {tag:?}.to_string(),\n"));
    }
    s.push_str("        ]);\n");
    s.push_str("        &TAGS\n");
    s.push_str("    }\n");

    // Supported functions
    s.push_str("    fn supported_functions(&self) -> &[crate::models::base::ModelFunction] {\n");
    s.push_str("        static FUNCS: std::sync::LazyLock<Vec<crate::models::base::ModelFunction>> = std::sync::LazyLock::new(|| vec![\n");
    for func in &model.supported_functions {
        s.push_str(&format!(
            "            crate::models::base::ModelFunction::{func},\n"
        ));
    }
    s.push_str("        ]);\n");
    s.push_str("        &FUNCS\n");
    s.push_str("    }\n");

    // Resolved provider config, if this instance was constructed from one.
    s.push_str("    fn provider_config(&self) -> Option<&crate::config::ProviderConfig> {\n");
    s.push_str("        self.provider_config.as_ref()\n");
    s.push_str("    }\n");
    s.push_str("}\n\n");

    // HasModelMetadata implementation
    s.push_str(&format!(
        "impl crate::models::base::HasModelMetadata for {struct_name} {{\n"
    ));
    s.push_str("    fn metadata() -> crate::models::base::ModelMetadata {\n");
    s.push_str(&generate_metadata_literal(model));
    s.push_str("    }\n");
    s.push_str("}\n\n");

    s
}

fn generate_metadata_literal(model: &YamlModel) -> String {
    let mut s = String::new();
    s.push_str("        crate::models::base::ModelMetadata {\n");
    s.push_str(&format!(
        "            family: {:?}.to_string(),\n",
        model.family
    ));
    s.push_str(&format!(
        "            version: {:?}.to_string(),\n",
        model.version
    ));
    s.push_str(&format!("            size: {},\n", model.size));
    s.push_str(&format!(
        "            context_length: {},\n",
        model.context_length
    ));
    s.push_str(&format!(
        "            model_type: crate::models::base::ModelType::{},\n",
        model.model_type
    ));
    s.push_str(&format!(
        "            huggingface_repo: {:?}.to_string(),\n",
        model.huggingface_repo
    ));
    s.push_str(&format!(
        "            native_dtype: {:?}.to_string(),\n",
        model.native_dtype
    ));
    s.push_str("            architecture: ");
    s.push_str(&generate_architecture_literal(&model.architecture));
    s.push_str(",\n");

    // Variants
    s.push_str("            variants: vec![\n");
    for variant in &model.variants {
        s.push_str("                crate::models::base::ModelVariant {\n");
        s.push_str(&format!(
            "                    format: {:?}.to_string(),\n",
            variant.format
        ));
        s.push_str(&format!(
            "                    precision: {:?}.to_string(),\n",
            variant.precision
        ));
        s.push_str(&format!(
            "                    size_gb: {},\n",
            format_float(variant.size_gb)
        ));
        s.push_str(&format!(
            "                    url: {:?}.to_string(),\n",
            variant.url
        ));
        s.push_str("                },\n");
    }
    s.push_str("            ],\n");

    // Description
    if let Some(ref desc) = model.description {
        s.push_str(&format!(
            "            description: Some({desc:?}.to_string()),\n"
        ));
    } else {
        s.push_str("            description: None,\n");
    }

    // Tags
    s.push_str("            tags: vec![\n");
    for tag in &model.tags {
        s.push_str(&format!("                {tag:?}.to_string(),\n"));
    }
    s.push_str("            ],\n");

    // Supported functions
    s.push_str("            supported_functions: vec![\n");
    for func in &model.supported_functions {
        s.push_str(&format!(
            "                crate::models::base::ModelFunction::{func},\n"
        ));
    }
    s.push_str("            ],\n");

    s.push_str("        }\n");
    s
}

fn generate_architecture_literal(arch: &YamlArchitecture) -> String {
    let mut s = String::new();
    s.push_str("crate::models::base::ModelArchitecture {\n");
    s.push_str(&format!(
        "            num_hidden_layers: {},\n",
        arch.num_hidden_layers
    ));
    s.push_str(&format!("            hidden_size: {},\n", arch.hidden_size));
    s.push_str(&format!(
        "            num_attention_heads: {},\n",
        arch.num_attention_heads
    ));
    s.push_str(&format!(
        "            num_key_value_heads: {},\n",
        arch.num_key_value_heads
    ));
    s.push_str(&format!("            head_dim: {},\n", arch.head_dim));
    s.push_str("            layer_types: vec![\n");
    for ltc in &arch.layer_types {
        s.push_str("                crate::models::base::LayerTypeCount {\n");
        s.push_str(&format!(
            "                    kind: {},\n",
            generate_layer_kind_literal(ltc)
        ));
        s.push_str(&format!("                    count: {},\n", ltc.count));
        s.push_str("                },\n");
    }
    s.push_str("            ],\n");
    s.push_str("        }");
    s
}

fn generate_layer_kind_literal(ltc: &YamlLayerTypeCount) -> String {
    match ltc.kind.as_str() {
        "full_attention" => "crate::models::base::LayerKind::FullAttention".to_string(),
        "sliding_attention" => {
            let window = ltc
                .window
                .unwrap_or_else(|| panic!("sliding_attention layer_type missing `window` field"));
            format!("crate::models::base::LayerKind::SlidingAttention {{ window: {window} }}")
        }
        "recurrent" => {
            let mamba = ltc
                .mamba
                .as_ref()
                .unwrap_or_else(|| panic!("recurrent layer_type missing `mamba` shape"));
            format!(
                "crate::models::base::LayerKind::Recurrent(crate::models::base::MambaShape {{ d_conv: {}, d_state: {}, d_inner: {}, n_groups: {} }})",
                mamba.d_conv, mamba.d_state, mamba.d_inner, mamba.n_groups
            )
        }
        other => panic!("Unknown layer_types kind: {other:?}"),
    }
}

fn format_float(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

fn model_id_to_struct_name(id: &str) -> String {
    // Convert "granite-3.1-3b-instruct" to "Granite313bInstruct"
    id.split('-')
        .map(|part| {
            if part.contains('.') {
                // Remove dots: "3.1" -> "31"
                part.replace('.', "")
            } else {
                // Capitalize first letter: "granite" -> "Granite"
                let mut chars = part.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                }
            }
        })
        .collect::<String>() // Join without separator for proper CamelCase
}

#[derive(serde::Deserialize)]
struct YamlModel {
    id: String,
    family: String,
    version: String,
    size: u64,
    context_length: u64,
    model_type: String,
    huggingface_repo: String,
    native_dtype: String,
    architecture: YamlArchitecture,
    variants: Vec<YamlModelVariant>,
    description: Option<String>,
    tags: Vec<String>,
    supported_functions: Vec<String>,
}

#[derive(serde::Deserialize)]
struct YamlModelVariant {
    format: String,
    precision: String,
    size_gb: f64,
    url: String,
}

#[derive(serde::Deserialize)]
struct YamlArchitecture {
    num_hidden_layers: u64,
    hidden_size: u64,
    num_attention_heads: u64,
    num_key_value_heads: u64,
    head_dim: u64,
    layer_types: Vec<YamlLayerTypeCount>,
}

#[derive(serde::Deserialize)]
struct YamlLayerTypeCount {
    kind: String,
    count: u64,
    window: Option<u64>,
    mamba: Option<YamlMambaShape>,
}

#[derive(serde::Deserialize)]
struct YamlMambaShape {
    d_conv: u64,
    d_state: u64,
    d_inner: u64,
    n_groups: u64,
}

/*-- version info --*/

struct VersionInfo {
    base_version: String,
    commit_hash: String,
    commits_since_tag: u32,
    has_uncommitted: bool,
}

fn get_version_info() -> VersionInfo {
    // Get the most recent vX.Y.Z tag
    let tag_output = Command::new("git")
        .args(["tag", "--sort=-v:refname"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        });

    let latest_tag = tag_output.as_ref().and_then(|tags| {
        tags.lines()
            .find(|line| {
                line.starts_with('v') && line[1..].chars().all(|c| c.is_ascii_digit() || c == '.')
            })
            .map(|s| s.to_string())
    });

    let base_version = latest_tag
        .as_ref()
        .map(|tag| tag.strip_prefix('v').unwrap_or(tag).to_string())
        .unwrap_or_else(|| "0.0.0".to_string());

    // Get short commit hash
    let commit_hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Count commits since tag
    let commits_since_tag = if let Some(ref tag) = latest_tag {
        Command::new("git")
            .args(["rev-list", &format!("{tag}..HEAD"), "--count"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    String::from_utf8(output.stdout)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok())
                } else {
                    None
                }
            })
            .unwrap_or(0)
    } else {
        // No tags exist, so we're in dev mode
        1
    };

    // Check for uncommitted changes
    let has_uncommitted = Command::new("git")
        .args(["diff", "--quiet"])
        .status()
        .map(|status| !status.success())
        .unwrap_or(false);

    VersionInfo {
        base_version,
        commit_hash,
        commits_since_tag,
        has_uncommitted,
    }
}

fn generate_version_module(info: &VersionInfo) -> String {
    format!(
        r#"// Auto-generated by build.rs - do not edit

pub const VERSION: &str = "{}";
pub const COMMIT_HASH: &str = "{}";
pub const COMMITS_SINCE_TAG: u32 = {};
pub const HAS_UNCOMMITTED: bool = {};

pub fn version_string() -> String {{
    let mut version = VERSION.to_string();

    if COMMITS_SINCE_TAG > 0 {{
        version.push_str("+dev");
    }}

    if HAS_UNCOMMITTED {{
        version.push_str("+dirty");
    }}

    let mut commit = format!("commit: {{COMMIT_HASH}}");
    if HAS_UNCOMMITTED {{
        commit.push_str("+dirty");
    }}

    format!("{{version}} ({{commit}})")
}}
"#,
        info.base_version, info.commit_hash, info.commits_since_tag, info.has_uncommitted
    )
}
