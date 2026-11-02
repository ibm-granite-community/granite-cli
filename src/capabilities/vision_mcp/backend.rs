//! VLM backend: a thin OpenAI-compatible vision/chat REST client.

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use url::Url;

/*-- Errors --------------------------------------------------------------------*/

#[derive(Debug, thiserror::Error)]
pub(crate) enum VlmError {
    #[error("API error: {0}")]
    ApiError(String),
    #[error("Invalid image data: {0}")]
    InvalidImage(String),
    #[error("Image exceeds size limit: {limit} bytes")]
    ImageTooLarge { limit: u64 },
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Rate limited: {0}")]
    RateLimited(String),
}

/*-- Analysis Types --------------------------------------------------------------*/

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnalysisType {
    ObjectDetection,
    TextExtraction,
    SceneDescription,
    UiAnalysis,
    Custom(String),
}

impl fmt::Display for AnalysisType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnalysisType::ObjectDetection => write!(f, "object-detection"),
            AnalysisType::TextExtraction => write!(f, "text-extraction"),
            AnalysisType::SceneDescription => write!(f, "scene-description"),
            AnalysisType::UiAnalysis => write!(f, "ui-analysis"),
            AnalysisType::Custom(_) => write!(f, "custom"),
        }
    }
}

/*-- Image Source ----------------------------------------------------------------*/

#[derive(Debug, Clone)]
pub(crate) enum ImageSource {
    Bytes { data: Vec<u8>, mime: String },
    File(PathBuf),
    Url(Url),
}

impl ImageSource {
    fn mime(&self) -> &str {
        match self {
            ImageSource::Bytes { mime, .. } => mime.as_str(),
            ImageSource::File(path) => match path.extension().and_then(|e| e.to_str()) {
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("png") => "image/png",
                Some("webp") => "image/webp",
                Some("gif") => "image/gif",
                Some("bmp") => "image/bmp",
                Some("tiff") | Some("tif") => "image/tiff",
                _ => "image/jpeg",
            },
            ImageSource::Url(url) => match url.path().rsplit('/').next().unwrap_or("") {
                "jpg" | "jpeg" => "image/jpeg",
                "png" => "image/png",
                "webp" => "image/webp",
                "gif" => "image/gif",
                "bmp" => "image/bmp",
                _ => "image/jpeg",
            },
        }
    }

    /// Reads the image into memory, enforcing `limit` for every source kind
    async fn to_bytes(&self, limit: u64) -> Result<(Vec<u8>, String), VlmError> {
        match self {
            ImageSource::Bytes { data, mime } => {
                if data.len() as u64 > limit {
                    return Err(VlmError::ImageTooLarge { limit });
                }
                Ok((data.clone(), mime.clone()))
            }
            ImageSource::File(path) => {
                let meta = std::fs::metadata(path).map_err(|e| {
                    VlmError::InvalidImage(format!("Failed to read file metadata: {e}"))
                })?;
                if meta.len() > limit {
                    return Err(VlmError::ImageTooLarge { limit });
                }
                let data = tokio::fs::read(path)
                    .await
                    .map_err(|e| VlmError::InvalidImage(format!("Failed to read file: {e}")))?;
                Ok((data, self.mime().to_string()))
            }
            ImageSource::Url(url) => {
                let resp = reqwest::get(url.clone())
                    .await
                    .map_err(|e| VlmError::Connection(e.to_string()))?;
                if let Some(len) = resp.content_length()
                    && len > limit
                {
                    return Err(VlmError::ImageTooLarge { limit });
                }
                let data = resp
                    .bytes()
                    .await
                    .map_err(|e| VlmError::Connection(e.to_string()))?;
                if data.len() as u64 > limit {
                    return Err(VlmError::ImageTooLarge { limit });
                }
                Ok((data.to_vec(), self.mime().to_string()))
            }
        }
    }

    async fn to_data_uri(&self, limit: u64) -> Result<String, VlmError> {
        let (data, mime) = self.to_bytes(limit).await?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        Ok(format!("data:{mime};base64,{encoded}"))
    }
}

/*-- VLM Backend Trait -------------------------------------------------------------*/

#[async_trait]
pub(crate) trait VlmBackend: Send + Sync + fmt::Debug {
    async fn compare_images(
        &self,
        images: Vec<ImageSource>,
        prompt: Option<String>,
    ) -> Result<String, VlmError>;

    async fn analyze(
        &self,
        image: ImageSource,
        analysis_type: AnalysisType,
        prompt: Option<String>,
    ) -> Result<String, VlmError>;
}

/*-- OpenAI-compatible chat wire types --------------------------------------------*/

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: &'static str,
    content: ChatContent,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Multi(Vec<ContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ContentImageUrl },
}

#[derive(Debug, Serialize)]
struct ContentImageUrl {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
}

/*-- OpenAiCompatibleVlm -----------------------------------------------------------*/

#[derive(Debug)]
pub(crate) struct OpenAiCompatibleVlm {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    max_image_bytes: u64,
    extra_headers: HashMap<String, String>,
}

impl OpenAiCompatibleVlm {
    pub(crate) fn new(
        endpoint: String,
        model: String,
        api_key: String,
        timeout_seconds: u64,
        max_image_bytes: u64,
        extra_headers: HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_seconds))
            .build()?;
        Ok(Self {
            client,
            endpoint,
            model,
            api_key,
            max_image_bytes,
            extra_headers,
        })
    }

    fn build_endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.endpoint.trim_end_matches('/'), path)
    }

    async fn chat_text(&self, system: &str, content: ChatContent) -> Result<String, VlmError> {
        let url = self.build_endpoint("chat/completions");

        let mut builder = self.client.post(&url);
        builder = builder.header("Content-Type", "application/json");
        if !self.api_key.is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", self.api_key));
        }
        for (k, v) in &self.extra_headers {
            builder = builder.header(k, v);
        }

        let messages = vec![
            ChatMessage {
                role: "system",
                content: ChatContent::Text(system.to_string()),
            },
            ChatMessage {
                role: "user",
                content,
            },
        ];

        let req = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: false,
            max_tokens: Some(4096),
        };

        let resp = builder
            .json(&req)
            .send()
            .await
            .map_err(|e| VlmError::Connection(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                return Err(VlmError::RateLimited(format!("Rate limited: {body}")));
            }
            return Err(VlmError::ApiError(format!("{status}: {body}")));
        }

        let body: ChatResponse = resp
            .json()
            .await
            .map_err(|e| VlmError::ApiError(format!("Failed to parse response: {e}")))?;

        body.choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| VlmError::ApiError("Empty response from VLM".to_string()))
    }

    async fn image_part(&self, image: &ImageSource) -> Result<ContentPart, VlmError> {
        Ok(ContentPart::ImageUrl {
            image_url: ContentImageUrl {
                url: image.to_data_uri(self.max_image_bytes).await?,
            },
        })
    }
}

#[async_trait]
impl VlmBackend for OpenAiCompatibleVlm {
    async fn compare_images(
        &self,
        images: Vec<ImageSource>,
        prompt: Option<String>,
    ) -> Result<String, VlmError> {
        if images.len() < 2 {
            return Err(VlmError::ApiError(
                "compare_images requires at least 2 images".to_string(),
            ));
        }
        let user_prompt = prompt.unwrap_or_else(|| {
            "Compare these images. Note similarities, differences, and any changes between them."
                .to_string()
        });

        let mut parts = Vec::new();
        for (i, img) in images.iter().enumerate() {
            let label = if images.len() == 2 {
                if i == 0 {
                    "First image"
                } else {
                    "Second image"
                }
                .to_string()
            } else {
                format!("Image {}", i + 1)
            };
            parts.push(ContentPart::Text { text: label });
            parts.push(self.image_part(img).await?);
        }
        parts.push(ContentPart::Text { text: user_prompt });

        self.chat_text(
            "You are a visual comparison assistant. Analyze differences and similarities between images.",
            ChatContent::Multi(parts),
        )
        .await
    }

    async fn analyze(
        &self,
        image: ImageSource,
        analysis_type: AnalysisType,
        prompt: Option<String>,
    ) -> Result<String, VlmError> {
        let (system_prompt, analysis_name) = match &analysis_type {
            AnalysisType::ObjectDetection => (
                "You are an object detection assistant. List all objects in the image with their positions and descriptions.",
                "object detection",
            ),
            AnalysisType::TextExtraction => (
                "You are a text extraction assistant. Transcribe all text in the image exactly as it appears.",
                "text extraction",
            ),
            AnalysisType::SceneDescription => (
                "You are a scene description assistant. Describe the overall scene, layout, atmosphere, and notable elements.",
                "scene description",
            ),
            AnalysisType::UiAnalysis => (
                "You are a UI analysis assistant. Identify all UI elements (buttons, text fields, icons, etc.) and describe their purpose and state.",
                "UI analysis",
            ),
            AnalysisType::Custom(p) => (p.as_str(), "custom analysis"),
        };
        let user_prompt = prompt.unwrap_or_else(|| match &analysis_type {
            AnalysisType::Custom(_) => {
                "Analyze this image according to the instructions above.".to_string()
            }
            _ => format!("Analyze this image for {analysis_name}."),
        });
        let content = ChatContent::Multi(vec![
            self.image_part(&image).await?,
            ContentPart::Text { text: user_prompt },
        ]);
        self.chat_text(system_prompt, content).await
    }
}
