//! Parsing helpers for the two shapes `ModelVariant.url` takes for
//! HuggingFace-hosted variants: a repo URL (safetensors/MLX variants, e.g.
//! `https://huggingface.co/{owner}/{repo}`) or a full blob URL (GGUF
//! variants, e.g.
//! `https://huggingface.co/{owner}/{repo}/blob/{branch}/{filename}`). A bare
//! `"owner/repo"` id is also accepted for robustness, though generated data
//! always uses a full URL.

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

use_channel!("HUGFC");

/// Extract `"owner/repo"` from a HF repo URL, a HF blob URL, or a bare repo
/// id. Returns `None` if `url` doesn't look like a HuggingFace reference
/// (e.g. an `ollama.com` library URL).
pub fn hf_repo_id(url: &str) -> Option<&str> {
    alog_channel!(MessageLevel::Debug2, "Analyzing URL: {}", url);
    let rest = if url.starts_with("https://huggingface.co/") {
        url.strip_prefix("https://huggingface.co/")?
    } else if url.starts_with("huggingface.co/") {
        url.strip_prefix("huggingface.co/")?
    } else if url.starts_with("https://hf.co/") {
        url.strip_prefix("https://hf.co/")?
    } else if url.starts_with("hf.co/") {
        url.strip_prefix("hf.co/")?
    } else {
        url
    };

    let mut parts = rest.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(&rest[..owner.len() + 1 + repo.len()])
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf_repo_id_from_bare_repo() {
        assert_eq!(
            hf_repo_id("ibm-granite/granite-speech-4.1-2b"),
            Some("ibm-granite/granite-speech-4.1-2b")
        );
    }

    #[test]
    fn hf_repo_id_from_repo_url() {
        assert_eq!(
            hf_repo_id("https://huggingface.co/ibm-granite/granite-speech-4.1-2b"),
            Some("ibm-granite/granite-speech-4.1-2b")
        );
    }

    #[test]
    fn hf_repo_id_from_blob_url() {
        assert_eq!(
            hf_repo_id(
                "https://huggingface.co/ibm-granite/granite-speech-4.1-2b-GGUF/blob/main/granite-speech-4.1-2b-Q4_K_M.gguf"
            ),
            Some("ibm-granite/granite-speech-4.1-2b-GGUF")
        );
    }

    #[test]
    fn hf_repo_id_rejects_non_hf_url() {
        assert_eq!(hf_repo_id("https://ollama.com/library/granite4:1b"), None);
    }
}
