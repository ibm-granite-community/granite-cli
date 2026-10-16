//! MCP tool definitions exposing a `VlmBackend` over `rmcp`'s `#[tool]`/
//! `#[tool_router]` macros.

use crate::capabilities::vision_mcp::backend::{AnalysisType, ImageSource, VlmBackend};
use base64::Engine;
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::ErrorCode, schemars, tool, tool_router,
};
use std::sync::Arc;
use url::Url;

/*-- ImageSource parsing -----------------------------------------------------------*/

trait ParseImage {
    fn source_type(&self) -> &str;
    fn image(&self) -> &str;

    fn parse_image(&self) -> Result<ImageSource, ErrorData> {
        match self.source_type() {
            "file" => Ok(ImageSource::File(std::path::PathBuf::from(self.image()))),
            "base64" => {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(self.image())
                    .map_err(|e| {
                        ErrorData::new(
                            ErrorCode::INVALID_PARAMS,
                            format!("Invalid base64: {e}"),
                            None,
                        )
                    })?;
                Ok(ImageSource::Bytes {
                    data,
                    mime: "image/jpeg".to_string(),
                })
            }
            "url" => {
                let url = Url::parse(self.image()).map_err(|e| {
                    ErrorData::new(ErrorCode::INVALID_PARAMS, format!("Invalid URL: {e}"), None)
                })?;
                Ok(ImageSource::Url(url))
            }
            other => Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("Unknown image source type: '{other}'. Use 'file', 'base64', or 'url'."),
                None,
            )),
        }
    }
}

macro_rules! impl_parse_image {
    ($ty:ty) => {
        impl ParseImage for $ty {
            fn source_type(&self) -> &str {
                &self.source_type
            }
            fn image(&self) -> &str {
                &self.image
            }
        }
    };
}

/*-- Tool argument types ------------------------------------------------------------*/

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImageSourceArgs {
    #[schemars(
        description = "Image source type: 'file' (path), 'base64' (data), or 'url' (HTTP(S))"
    )]
    pub source_type: String,
    /// The image: file path, base64-encoded data, or HTTP(S) URL
    pub image: String,
}
impl_parse_image!(ImageSourceArgs);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CompareImagesArgs {
    /// Array of image sources to compare (minimum 2)
    pub images: Vec<ImageSourceArgs>,
    /// Optional free-text instruction
    pub prompt: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AnalyzeArgs {
    #[schemars(description = "Image source type: 'file', 'base64', or 'url'")]
    pub source_type: String,
    pub image: String,
    #[schemars(
        description = "Analysis type: 'object-detection', 'text-extraction', 'scene-description', 'ui-analysis', or 'custom'"
    )]
    pub analysis_type: String,
    /// Optional instruction (required when analysis_type == 'custom')
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}
impl_parse_image!(AnalyzeArgs);

/*-- Tool router (MCP tools) --------------------------------------------------------*/

/// Wraps a VLM backend and exposes it as MCP tools via `#[tool_router]`.
#[derive(Debug, Clone)]
pub struct VlmToolRegistry {
    vlm: Arc<dyn VlmBackend>,
}

impl VlmToolRegistry {
    pub fn new(vlm: Arc<dyn VlmBackend>) -> Self {
        Self { vlm }
    }
}
#[tool_router(server_handler)]
impl VlmToolRegistry {
    #[tool(
        description = "Compare two or more images and note differences, similarities, and changes. Pass at least 2 images to compare."
    )]
    async fn vlm_compare_images(
        &self,
        Parameters(args): Parameters<CompareImagesArgs>,
    ) -> Result<String, ErrorData> {
        if args.images.len() < 2 {
            return Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "compare_images requires at least 2 images".to_string(),
                None,
            ));
        }
        let mut images = Vec::with_capacity(args.images.len());
        for (i, img_arg) in args.images.iter().enumerate() {
            images.push(img_arg.parse_image().map_err(|e| {
                ErrorData::new(
                    ErrorCode::INVALID_PARAMS,
                    format!("Image {}: {}", i + 1, e.message),
                    None,
                )
            })?);
        }
        self.vlm
            .compare_images(images, args.prompt)
            .await
            .map_err(|e| ErrorData::new(ErrorCode::INTERNAL_ERROR, e.to_string(), None))
    }

    #[tool(
        description = "Analyze an image for specific visual characteristics. Use 'object-detection' to list objects, 'text-extraction' for OCR, 'scene-description' for layout/atmosphere, 'ui-analysis' for UI elements, or 'custom' with a prompt for arbitrary analysis."
    )]
    async fn vlm_analyze(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<String, ErrorData> {
        let image = args.parse_image()?;
        let analysis_type = match args.analysis_type.as_str() {
            "object-detection" => AnalysisType::ObjectDetection,
            "text-extraction" => AnalysisType::TextExtraction,
            "scene-description" => AnalysisType::SceneDescription,
            "ui-analysis" => AnalysisType::UiAnalysis,
            "custom" => {
                let prompt = args.prompt.clone().ok_or_else(|| {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        "'custom' analysis requires a prompt".to_string(),
                        None,
                    )
                })?;
                AnalysisType::Custom(prompt)
            }
            other => {
                return Err(ErrorData::new(
                    ErrorCode::INVALID_PARAMS,
                    format!("Unknown analysis type: '{other}'"),
                    None,
                ));
            }
        };
        self.vlm
            .analyze(image, analysis_type, args.prompt)
            .await
            .map_err(|e| ErrorData::new(ErrorCode::INTERNAL_ERROR, e.to_string(), None))
    }
}

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::vision_mcp::backend::VlmError;
    use std::sync::Mutex;

    // Records tool calls so dispatch can be asserted without contacting a VLM service.
    #[derive(Debug, Default)]
    struct FakeVlm {
        analyze_calls: Mutex<Vec<(AnalysisType, Option<String>)>>,
        compare_calls: Mutex<Vec<(usize, Option<String>)>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl VlmBackend for FakeVlm {
        async fn compare_images(
            &self,
            images: Vec<ImageSource>,
            prompt: Option<String>,
        ) -> Result<String, VlmError> {
            self.compare_calls
                .lock()
                .unwrap()
                .push((images.len(), prompt));
            if self.fail {
                Err(VlmError::ApiError("backend failed".to_string()))
            } else {
                Ok("compared".to_string())
            }
        }

        async fn analyze(
            &self,
            _image: ImageSource,
            analysis_type: AnalysisType,
            prompt: Option<String>,
        ) -> Result<String, VlmError> {
            self.analyze_calls
                .lock()
                .unwrap()
                .push((analysis_type, prompt));
            if self.fail {
                Err(VlmError::ApiError("backend failed".to_string()))
            } else {
                Ok("analyzed".to_string())
            }
        }
    }

    fn image(source_type: &str, image: &str) -> ImageSourceArgs {
        ImageSourceArgs {
            source_type: source_type.to_string(),
            image: image.to_string(),
        }
    }

    fn make_registry(backend: Arc<FakeVlm>) -> VlmToolRegistry {
        VlmToolRegistry::new(backend)
    }

    // Each supported input form is converted to the corresponding backend source.
    #[test]
    fn parses_supported_image_sources() {
        assert!(matches!(
            image("file", "images/cat.png").parse_image().unwrap(),
            ImageSource::File(path) if path == std::path::Path::new("images/cat.png")
        ));
        assert!(matches!(
            image("base64", "aGVsbG8=").parse_image().unwrap(),
            ImageSource::Bytes { data, mime } if data == b"hello" && mime == "image/jpeg"
        ));
        assert!(matches!(
            image("url", "https://example.com/cat.png")
                .parse_image()
                .unwrap(),
            ImageSource::Url(url) if url.as_str() == "https://example.com/cat.png"
        ));
    }

    // Malformed data, malformed URLs, and unsupported source names are rejected.
    #[test]
    fn rejects_invalid_image_source_values() {
        let invalid_base64 = image("base64", "not base64!").parse_image().unwrap_err();
        assert!(invalid_base64.message.starts_with("Invalid base64:"));

        let invalid_url = image("url", "not a URL").parse_image().unwrap_err();
        assert!(invalid_url.message.starts_with("Invalid URL:"));

        let unknown = image("data-uri", "abc").parse_image().unwrap_err();
        assert!(
            unknown
                .message
                .contains("Unknown image source type: 'data-uri'")
        );
    }

    // Too few images and a bad later image fail before the backend is called.
    #[tokio::test]
    async fn compare_validates_inputs_and_labels_bad_images() {
        let backend = Arc::new(FakeVlm::default());
        let registry = make_registry(backend.clone());

        for images in [vec![], vec![image("file", "one.png")]] {
            let error = registry
                .vlm_compare_images(Parameters(CompareImagesArgs {
                    images,
                    prompt: None,
                }))
                .await
                .unwrap_err();
            assert!(error.message.contains("at least 2 images"));
        }

        let error = registry
            .vlm_compare_images(Parameters(CompareImagesArgs {
                images: vec![image("file", "one.png"), image("base64", "bad!")],
                prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(error.message.starts_with("Image 2: Invalid base64:"));
        assert!(backend.compare_calls.lock().unwrap().is_empty());
    }

    // Valid inputs reach the backend with the prompt and both parsed images.
    #[tokio::test]
    async fn compare_passes_images_and_prompt_to_backend() {
        let backend = Arc::new(FakeVlm::default());
        let registry = make_registry(backend.clone());
        let result = registry
            .vlm_compare_images(Parameters(CompareImagesArgs {
                images: vec![
                    image("file", "one.png"),
                    image("url", "https://example.com/two.jpg"),
                ],
                prompt: Some("Compare colors".to_string()),
            }))
            .await
            .unwrap();

        assert_eq!(result, "compared");
        assert_eq!(
            *backend.compare_calls.lock().unwrap(),
            vec![(2, Some("Compare colors".to_string()))]
        );
    }

    // Public analysis names map to backend variants, including custom instructions.
    #[tokio::test]
    async fn analyze_maps_builtin_and_custom_types() {
        let cases = [
            ("object-detection", AnalysisType::ObjectDetection, None),
            ("text-extraction", AnalysisType::TextExtraction, None),
            ("scene-description", AnalysisType::SceneDescription, None),
            ("ui-analysis", AnalysisType::UiAnalysis, None),
            (
                "custom",
                AnalysisType::Custom("look closely".to_string()),
                Some("look closely".to_string()),
            ),
        ];

        for (name, expected_type, prompt) in cases {
            let backend = Arc::new(FakeVlm::default());
            let registry = make_registry(backend.clone());
            assert_eq!(
                registry
                    .vlm_analyze(Parameters(AnalyzeArgs {
                        source_type: "file".to_string(),
                        image: "picture.png".to_string(),
                        analysis_type: name.to_string(),
                        prompt: prompt.clone(),
                    }))
                    .await
                    .unwrap(),
                "analyzed"
            );
            assert_eq!(
                *backend.analyze_calls.lock().unwrap(),
                vec![(expected_type, prompt)]
            );
        }
    }

    // Custom analysis needs a prompt, and unknown analysis names are rejected.
    #[tokio::test]
    async fn analyze_rejects_missing_custom_prompt_and_unknown_type() {
        let registry = make_registry(Arc::new(FakeVlm::default()));
        let missing_prompt = registry
            .vlm_analyze(Parameters(AnalyzeArgs {
                source_type: "file".to_string(),
                image: "picture.png".to_string(),
                analysis_type: "custom".to_string(),
                prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(missing_prompt.message.contains("requires a prompt"));

        let unknown = registry
            .vlm_analyze(Parameters(AnalyzeArgs {
                source_type: "file".to_string(),
                image: "picture.png".to_string(),
                analysis_type: "not-real".to_string(),
                prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(
            unknown
                .message
                .contains("Unknown analysis type: 'not-real'")
        );
    }

    // Errors from either backend operation are exposed as MCP internal errors.
    #[tokio::test]
    async fn maps_backend_errors_to_internal_errors() {
        let registry = make_registry(Arc::new(FakeVlm {
            fail: true,
            ..FakeVlm::default()
        }));
        let error = registry
            .vlm_analyze(Parameters(AnalyzeArgs {
                source_type: "file".to_string(),
                image: "picture.png".to_string(),
                analysis_type: "object-detection".to_string(),
                prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(error.message.contains("backend failed"));

        let registry = make_registry(Arc::new(FakeVlm {
            fail: true,
            ..FakeVlm::default()
        }));
        let error = registry
            .vlm_compare_images(Parameters(CompareImagesArgs {
                images: vec![image("file", "one.png"), image("file", "two.png")],
                prompt: None,
            }))
            .await
            .unwrap_err();
        assert!(error.message.contains("backend failed"));
    }
}
