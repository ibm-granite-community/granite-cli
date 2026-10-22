// Local
use crate::models::base::{LayerKind, ModelArchitecture, ModelVariant};
use crate::utils::hardware::HardwareProfile;

/*-- ContextFit ---------------------------------------------------------------*/

/// Whether a model variant fits on a given hardware profile: at its full
/// configured context length, at some reduced (but still useful) context
/// length, or not at all. `Partial` carries the max context length that
/// fits, floored to the nearest power of two (the estimate is approximate,
/// so a round number avoids implying false precision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFit {
    Full,
    Partial(u64),
    None,
}

impl std::fmt::Display for ContextFit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextFit::Full => write!(f, "Full"),
            ContextFit::Partial(max_context) => {
                write!(f, "Partial ({})", format_token_count(*max_context))
            }
            ContextFit::None => write!(f, "None"),
        }
    }
}

/// Human-readable token count using binary K/M suffixes. Callers only ever
/// pass powers of two (post-`floor_pow2`), so division is always exact.
fn format_token_count(tokens: u64) -> String {
    if tokens >= 1024 * 1024 {
        format!("{}M", tokens / (1024 * 1024))
    } else if tokens >= 1024 {
        format!("{}K", tokens / 1024)
    } else {
        tokens.to_string()
    }
}

/// Largest power of two `<= n` (with `n` treated as at least 1).
fn floor_pow2(n: u64) -> u64 {
    1u64 << (63 - n.max(1).leading_zeros())
}

/*-- estimation ----------------------------------------------------------------*/

// Extra headroom for runtime overhead beyond raw weights + KV cache
// (activations, framework overhead, fragmentation).
const RUNTIME_OVERHEAD_FACTOR: f64 = 1.15;

// A "partial" fit must allow at least this fraction of the model's configured
// context length -- otherwise it's not a useful result.
const MIN_USABLE_CONTEXT_FRACTION: f64 = 0.1;
const MIN_USABLE_CONTEXT_TOKENS_FLOOR: u64 = 2048;

/// Estimate whether `variant` will fit on `hardware`, and at what fraction of
/// `context_length`. Derives KV-cache/recurrent-state memory from the
/// model's actual per-layer-kind architecture (attention head shape,
/// sliding-window sizes, Mamba/SSM state shapes) rather than a flat
/// per-parameter heuristic.
pub(crate) fn estimate(
    context_length: u64,
    architecture: &ModelArchitecture,
    native_dtype: &str,
    variant: &ModelVariant,
    hardware: &HardwareProfile,
) -> ContextFit {
    if context_length == 0 || architecture.layer_types.is_empty() {
        return ContextFit::None;
    }

    let usable_gb = hardware.usable_memory_gb();
    if usable_gb <= variant.size_gb.unwrap_or(f64::MAX) {
        return ContextFit::None;
    }

    if required_gb(architecture, variant, native_dtype, context_length) <= usable_gb {
        return ContextFit::Full;
    }

    let max_context_tokens = max_context_fitting(
        architecture,
        variant,
        native_dtype,
        usable_gb,
        context_length,
    );
    let min_useful_context = ((context_length as f64 * MIN_USABLE_CONTEXT_FRACTION) as u64)
        .max(MIN_USABLE_CONTEXT_TOKENS_FLOOR)
        .min(context_length);

    if max_context_tokens >= min_useful_context {
        ContextFit::Partial(floor_pow2(max_context_tokens))
    } else {
        ContextFit::None
    }
}

/// Total memory (weights + KV cache / recurrent state, with runtime
/// overhead) required to run `variant` at `context_tokens` tokens of
/// context, in gigabytes. Exposed beyond this module (e.g. to the setup
/// wizard) so a variant's estimated VRAM/RAM footprint can be shown
/// alongside it wherever the model's `ModelArchitecture` and `native_dtype`
/// are available -- most usefully at `context_tokens = model.context_length`
/// for a "full context" estimate.
pub fn required_gb(
    architecture: &ModelArchitecture,
    variant: &ModelVariant,
    native_dtype: &str,
    context_tokens: u64,
) -> f64 {
    let attn_bytes_per_elem = cache_bytes_per_elem_attention(&variant.format, native_dtype);
    let recurrent_bytes_per_elem = cache_bytes_per_elem_recurrent(&variant.format, native_dtype);
    let kv_bytes_per_token = 2.0
        * architecture.num_key_value_heads as f64
        * architecture.head_dim as f64
        * attn_bytes_per_elem;

    let layers_bytes: f64 = architecture
        .layer_types
        .iter()
        .map(|ltc| {
            ltc.count as f64
                * layer_kind_bytes(
                    &ltc.kind,
                    context_tokens,
                    kv_bytes_per_token,
                    recurrent_bytes_per_elem,
                )
        })
        .sum();

    (variant.size_gb.unwrap_or(f64::MAX) + layers_bytes / 1e9) * RUNTIME_OVERHEAD_FACTOR
}

/// Bytes of KV-cache/recurrent-state memory for a single layer of `kind` at
/// `context_tokens` tokens of context. `required_gb`'s monotonicity in
/// `context_tokens` (needed for the binary search below) depends on every
/// arm here being non-decreasing in `context_tokens`.
fn layer_kind_bytes(
    kind: &LayerKind,
    context_tokens: u64,
    kv_bytes_per_token: f64,
    recurrent_bytes_per_elem: f64,
) -> f64 {
    match kind {
        LayerKind::FullAttention => kv_bytes_per_token * context_tokens as f64,
        LayerKind::SlidingAttention { window } => {
            kv_bytes_per_token * context_tokens.min(*window) as f64
        }
        LayerKind::Recurrent(shape) => {
            // Fixed-size state, independent of context length: conv-state +
            // SSM-state element counts, per the Mamba2/SSM cache-sizing
            // formula (see llama.cpp's recurrent-state handling).
            let conv_state_elems = shape.d_conv.saturating_sub(1)
                * (shape.d_inner + 2 * shape.n_groups * shape.d_state);
            let ssm_state_elems = shape.d_state * shape.d_inner;
            (conv_state_elems + ssm_state_elems) as f64 * recurrent_bytes_per_elem
        }
    }
}

/// Bytes per element for an attention KV-cache entry. GGUF/Ollama inference
/// engines (llama.cpp) run the KV cache at a fixed fp16 regardless of the
/// checkpoint's native dtype; safetensors serving keeps the cache at
/// whatever dtype the model loads in.
fn cache_bytes_per_elem_attention(format: &str, native_dtype: &str) -> f64 {
    if format.eq_ignore_ascii_case("safetensors") {
        bytes_per_elem_for_dtype(native_dtype)
    } else {
        2.0
    }
}

/// Bytes per element for a recurrent (Mamba/SSM) layer's fixed state.
/// llama.cpp hardcodes this state to F32 regardless of attention KV-cache
/// precision; for safetensors serving this is an unverified best guess
/// (recurrent state is a small fraction of total memory, so imprecision
/// here has little effect on the overall estimate).
fn cache_bytes_per_elem_recurrent(format: &str, native_dtype: &str) -> f64 {
    if format.eq_ignore_ascii_case("safetensors") {
        bytes_per_elem_for_dtype(native_dtype)
    } else {
        4.0
    }
}

fn bytes_per_elem_for_dtype(native_dtype: &str) -> f64 {
    match native_dtype {
        "bfloat16" | "float16" => 2.0,
        "float32" => 4.0,
        _ => 2.0,
    }
}

/// Largest `context_tokens` in `0..=context_length` for which
/// `required_gb(...) <= usable_gb`, found via binary search since
/// `required_gb` is monotonic non-decreasing in `context_tokens` for every
/// known `LayerKind` (this generalizes across dense, hybrid, and
/// sliding-window architectures without a per-shape closed-form inversion).
fn max_context_fitting(
    architecture: &ModelArchitecture,
    variant: &ModelVariant,
    native_dtype: &str,
    usable_gb: f64,
    context_length: u64,
) -> u64 {
    let fits = |tokens: u64| required_gb(architecture, variant, native_dtype, tokens) <= usable_gb;

    if !fits(0) {
        return 0;
    }

    let mut lo = 0u64;
    let mut hi = context_length;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::base::{LayerTypeCount, MambaShape};
    use crate::utils::hardware::Memory;

    fn hardware_with_vram(vram_gb: f64) -> HardwareProfile {
        hardware(Memory::Discrete {
            vram_gb,
            ram_gb: Some(32.0),
        })
    }

    fn hardware(memory: Memory) -> HardwareProfile {
        let mut profile = HardwareProfile::with_memory(memory);
        profile.gpu_vendor = Some("NVIDIA".to_string());
        profile
    }

    fn variant(format: &str, size_gb: f64) -> ModelVariant {
        ModelVariant {
            format: format.to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(size_gb),
            url: "http://example.com/model.gguf".to_string(),
        }
    }

    fn dense_architecture() -> ModelArchitecture {
        // granite-3.3-8b-instruct shape
        ModelArchitecture {
            num_hidden_layers: 40,
            hidden_size: 4096,
            num_attention_heads: 32,
            num_key_value_heads: 8,
            head_dim: 128,
            layer_types: vec![LayerTypeCount {
                kind: LayerKind::FullAttention,
                count: 40,
            }],
        }
    }

    fn hybrid_architecture() -> ModelArchitecture {
        // granite-4.0-h-1b shape
        ModelArchitecture {
            num_hidden_layers: 40,
            hidden_size: 1536,
            num_attention_heads: 12,
            num_key_value_heads: 4,
            head_dim: 128,
            layer_types: vec![
                LayerTypeCount {
                    kind: LayerKind::FullAttention,
                    count: 4,
                },
                LayerTypeCount {
                    kind: LayerKind::Recurrent(MambaShape {
                        d_conv: 4,
                        d_state: 128,
                        d_inner: 3072,
                        n_groups: 1,
                    }),
                    count: 36,
                },
            ],
        }
    }

    fn swa_architecture() -> ModelArchitecture {
        // granite-swash-2b shape
        ModelArchitecture {
            num_hidden_layers: 24,
            hidden_size: 2560,
            num_attention_heads: 20,
            num_key_value_heads: 4,
            head_dim: 128,
            layer_types: vec![
                LayerTypeCount {
                    kind: LayerKind::FullAttention,
                    count: 7,
                },
                LayerTypeCount {
                    kind: LayerKind::SlidingAttention { window: 128 },
                    count: 17,
                },
            ],
        }
    }

    #[test]
    fn recurrent_layer_bytes_match_mamba2_formula() {
        // conv_state = (4-1) * (3072 + 2*1*128) = 3 * 3328 = 9984
        // ssm_state  = 128 * 3072 = 393216
        // total elems = 403200, * 4 bytes (F32) = 1612800 bytes
        let shape = MambaShape {
            d_conv: 4,
            d_state: 128,
            d_inner: 3072,
            n_groups: 1,
        };
        let bytes = layer_kind_bytes(&LayerKind::Recurrent(shape), 100_000, 999.0, 4.0);
        assert_eq!(bytes, 1_612_800.0);
    }

    #[test]
    fn recurrent_layer_bytes_ignore_context_length() {
        let shape = MambaShape {
            d_conv: 4,
            d_state: 128,
            d_inner: 3072,
            n_groups: 1,
        };
        let short = layer_kind_bytes(&LayerKind::Recurrent(shape.clone()), 100, 999.0, 4.0);
        let long = layer_kind_bytes(&LayerKind::Recurrent(shape), 1_000_000, 999.0, 4.0);
        assert_eq!(short, long);
    }

    #[test]
    fn sliding_attention_caps_at_window() {
        let below_window =
            layer_kind_bytes(&LayerKind::SlidingAttention { window: 128 }, 64, 10.0, 4.0);
        let at_window =
            layer_kind_bytes(&LayerKind::SlidingAttention { window: 128 }, 128, 10.0, 4.0);
        let above_window = layer_kind_bytes(
            &LayerKind::SlidingAttention { window: 128 },
            1_000_000,
            10.0,
            4.0,
        );
        assert_eq!(below_window, 640.0);
        assert_eq!(at_window, 1280.0);
        assert_eq!(above_window, 1280.0);
    }

    #[test]
    fn full_attention_scales_linearly_with_context() {
        let a = layer_kind_bytes(&LayerKind::FullAttention, 1000, 2.0, 4.0);
        let b = layer_kind_bytes(&LayerKind::FullAttention, 2000, 2.0, 4.0);
        assert_eq!(a, 2000.0);
        assert_eq!(b, 4000.0);
    }

    #[test]
    fn hybrid_model_requires_far_less_memory_than_dense_equivalent_at_long_context() {
        let variant = variant("GGUF", 2.0);
        let long_context = 100_000;
        let dense_gb = required_gb(&dense_architecture(), &variant, "bfloat16", long_context);
        let hybrid_gb = required_gb(&hybrid_architecture(), &variant, "bfloat16", long_context);
        assert!(
            hybrid_gb < dense_gb / 2.0,
            "hybrid ({hybrid_gb} GB) should be far cheaper than dense ({dense_gb} GB) at long context"
        );
    }

    #[test]
    fn ample_vram_yields_full_fit() {
        let hardware = hardware_with_vram(256.0);
        let fit = estimate(
            131_072,
            &dense_architecture(),
            "bfloat16",
            &variant("GGUF", 5.0),
            &hardware,
        );
        assert_eq!(fit, ContextFit::Full);
    }

    #[test]
    fn insufficient_vram_for_weights_alone_yields_none() {
        let hardware = hardware_with_vram(4.0);
        let fit = estimate(
            131_072,
            &dense_architecture(),
            "bfloat16",
            &variant("GGUF", 5.0),
            &hardware,
        );
        assert_eq!(fit, ContextFit::None);
    }

    #[test]
    fn tight_vram_yields_partial_fit_for_dense_model() {
        // Enough for weights + a meaningfully reduced dense KV cache, but
        // not the full 131072-token context.
        let hardware = hardware_with_vram(22.5);
        let fit = estimate(
            131_072,
            &dense_architecture(),
            "bfloat16",
            &variant("GGUF", 5.0),
            &hardware,
        );
        assert!(matches!(fit, ContextFit::Partial(_)));
    }

    #[test]
    fn each_memory_kind_estimates_from_the_figure_it_names() {
        // 22.5 GB of accelerator memory and 30 GB of system RAM both leave
        // 15 GB usable, which gives a partial fit for this model.
        let fit = |hardware: &HardwareProfile| {
            estimate(
                131_072,
                &dense_architecture(),
                "bfloat16",
                &variant("GGUF", 5.0),
                hardware,
            )
        };
        let discrete = fit(&hardware(Memory::Discrete {
            vram_gb: 22.5,
            ram_gb: Some(128.0),
        }));
        let unified = fit(&hardware(Memory::Unified {
            vram_gb: 22.5,
            ram_gb: Some(128.0),
        }));
        let system = fit(&hardware(Memory::System { ram_gb: 30.0 }));

        assert!(matches!(discrete, ContextFit::Partial(_)));
        assert_eq!(unified, discrete);
        assert_eq!(system, discrete);
    }

    #[test]
    fn a_set_headroom_factor_changes_the_estimate() {
        let fit = |hardware: &HardwareProfile| {
            estimate(
                131_072,
                &dense_architecture(),
                "bfloat16",
                &variant("GGUF", 5.0),
                hardware,
            )
        };
        let default = hardware_with_vram(22.5);
        let mut whole_card = hardware_with_vram(22.5);
        whole_card.headroom = Some(1.0);

        assert!(matches!(fit(&default), ContextFit::Partial(_)));
        assert_ne!(fit(&whole_card), fit(&default));
    }

    #[test]
    fn partial_fit_carries_max_context_as_power_of_two() {
        let hardware = hardware_with_vram(22.5);
        let fit = estimate(
            131_072,
            &dense_architecture(),
            "bfloat16",
            &variant("GGUF", 5.0),
            &hardware,
        );
        match fit {
            ContextFit::Partial(max_context) => {
                assert!(max_context.is_power_of_two());
                assert!(max_context <= 131_072);
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    #[test]
    fn floor_pow2_rounds_down_to_nearest_power_of_two() {
        assert_eq!(floor_pow2(17_249), 16_384);
        assert_eq!(floor_pow2(1), 1);
        assert_eq!(floor_pow2(2), 2);
        assert_eq!(floor_pow2(4096), 4096);
        assert_eq!(floor_pow2(4097), 4096);
    }

    #[test]
    fn hybrid_model_fits_full_context_where_dense_equivalent_would_not() {
        let hardware = hardware_with_vram(6.0);
        let variant = variant("GGUF", 2.0);
        let dense_fit = estimate(
            131_072,
            &dense_architecture(),
            "bfloat16",
            &variant,
            &hardware,
        );
        let hybrid_fit = estimate(
            131_072,
            &hybrid_architecture(),
            "bfloat16",
            &variant,
            &hardware,
        );
        assert_ne!(dense_fit, ContextFit::Full);
        assert_eq!(hybrid_fit, ContextFit::Full);
    }

    #[test]
    fn swa_model_fits_full_context_where_dense_equivalent_would_not() {
        let hardware = hardware_with_vram(7.5);
        let variant = variant("GGUF", 2.0);
        let dense_fit = estimate(
            131_072,
            &dense_architecture(),
            "bfloat16",
            &variant,
            &hardware,
        );
        let swa_fit = estimate(
            131_072,
            &swa_architecture(),
            "bfloat16",
            &variant,
            &hardware,
        );
        assert_ne!(dense_fit, ContextFit::Full);
        assert_eq!(swa_fit, ContextFit::Full);
    }

    #[test]
    fn zero_context_length_yields_none() {
        let hardware = hardware_with_vram(256.0);
        let fit = estimate(
            0,
            &dense_architecture(),
            "bfloat16",
            &variant("GGUF", 5.0),
            &hardware,
        );
        assert_eq!(fit, ContextFit::None);
    }

    #[test]
    fn empty_layer_types_yields_none() {
        let hardware = hardware_with_vram(256.0);
        let empty_architecture = ModelArchitecture {
            num_hidden_layers: 0,
            hidden_size: 0,
            num_attention_heads: 0,
            num_key_value_heads: 0,
            head_dim: 0,
            layer_types: vec![],
        };
        let fit = estimate(
            4096,
            &empty_architecture,
            "bfloat16",
            &variant("GGUF", 5.0),
            &hardware,
        );
        assert_eq!(fit, ContextFit::None);
    }

    #[test]
    fn safetensors_uses_native_dtype_for_kv_cache_precision() {
        let architecture = dense_architecture();
        let variant = variant("safetensors", 16.0);
        let bf16_gb = required_gb(&architecture, &variant, "bfloat16", 100_000);
        let fp32_gb = required_gb(&architecture, &variant, "float32", 100_000);
        assert!(
            fp32_gb > bf16_gb,
            "float32 KV cache should require more memory than bfloat16"
        );
    }

    #[test]
    fn max_context_fitting_is_monotonic_and_bounded() {
        let architecture = dense_architecture();
        let variant = variant("GGUF", 5.0);
        let usable_gb = 8.0;
        let max_tokens =
            max_context_fitting(&architecture, &variant, "bfloat16", usable_gb, 131_072);
        assert!(max_tokens <= 131_072);
        assert!(required_gb(&architecture, &variant, "bfloat16", max_tokens) <= usable_gb);
        if max_tokens < 131_072 {
            assert!(required_gb(&architecture, &variant, "bfloat16", max_tokens + 1) > usable_gb);
        }
    }
}
