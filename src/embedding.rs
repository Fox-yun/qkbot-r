//! Bounded, single-item SiliconFlow VL embeddings. No provider body enters errors.
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const TEMPLATE_VERSION: &str = "qkbot-raw-text-inline-jpeg-float-le-v1";
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_JPEG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub embeddings_url: String,
    pub api_key: String,
    pub model: String,
    pub dimensions: usize,
    pub request_timeout_secs: u64,
    pub max_response_bytes: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embeddings_url: "https://api.siliconflow.cn/v1/embeddings".into(),
            api_key: String::new(),
            model: "Qwen/Qwen3-VL-Embedding-8B".into(),
            dimensions: 1024,
            request_timeout_secs: 15,
            max_response_bytes: 256 * 1024,
        }
    }
}

impl std::fmt::Debug for EmbeddingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingConfig")
            .field("enabled", &self.enabled)
            // An invalid, not-yet-validated endpoint might contain a credential.
            .field("embeddings_url", &"[configured]")
            .field("api_key", &"[redacted]")
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingError {
    Disabled,
    Configuration,
    Input,
    Cancelled,
    Timeout,
    SourceChanged,
    SchedulerClosed,
    Transport,
    HttpStatus(u16),
    ResponseTooLarge,
    Response,
    Vector,
}

impl std::fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpStatus(status) => write!(f, "embedding_http_status_{status}"),
            Self::Disabled => f.write_str("embedding_disabled"),
            Self::Configuration => f.write_str("embedding_invalid_configuration"),
            Self::Input => f.write_str("embedding_invalid_input"),
            Self::Cancelled => f.write_str("embedding_cancelled"),
            Self::Timeout => f.write_str("embedding_timeout"),
            Self::SourceChanged => f.write_str("embedding_source_changed"),
            Self::SchedulerClosed => f.write_str("embedding_scheduler_closed"),
            Self::Transport => f.write_str("embedding_transport_failed"),
            Self::ResponseTooLarge => f.write_str("embedding_response_too_large"),
            Self::Response => f.write_str("embedding_invalid_response"),
            Self::Vector => f.write_str("embedding_invalid_vector"),
        }
    }
}

impl std::error::Error for EmbeddingError {}

impl EmbeddingConfig {
    pub fn validate(&self) -> Result<()> {
        self.endpoint()?;
        if !(64..=4096).contains(&self.dimensions)
            || !(1..=30).contains(&self.request_timeout_secs)
            || !(1024..=1024 * 1024).contains(&self.max_response_bytes)
            || self.model.trim().is_empty()
            || self.model.trim() != self.model
            || self.model.len() > 256
            || self.model.chars().any(char::is_control)
            || self.api_key.len() > 8192
        {
            return Err(EmbeddingError::Configuration.into());
        }
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.api_key))
            .map_err(|_| EmbeddingError::Configuration)?;
        Ok(())
    }

    fn endpoint(&self) -> Result<reqwest::Url> {
        if self.embeddings_url.len() > 2048
            || self.embeddings_url.trim() != self.embeddings_url
            || self.embeddings_url.chars().any(char::is_control)
        {
            return Err(EmbeddingError::Configuration.into());
        }
        let endpoint =
            reqwest::Url::parse(&self.embeddings_url).map_err(|_| EmbeddingError::Configuration)?;
        // Cleartext is available only for local compatibility tests/services.
        let loopback_http = endpoint.scheme() == "http"
            && endpoint
                .host_str()
                .and_then(|host| {
                    host.trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|ip| ip.is_loopback());
        if !(endpoint.scheme() == "https" || loopback_http)
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(EmbeddingError::Configuration.into());
        }
        Ok(endpoint)
    }
}

pub struct EmbeddingResponse {
    /// Finite, nonzero, unit-length vector in this client's fingerprint space.
    pub vector: Vec<f32>,
    pub input_tokens: u64,
}

impl std::fmt::Debug for EmbeddingResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingResponse")
            .field("dimensions", &self.vector.len())
            .field("input_tokens", &self.input_tokens)
            .finish()
    }
}

#[derive(Clone)]
pub struct Embedding {
    config: EmbeddingConfig,
    endpoint: reqwest::Url,
    fingerprint: String,
    http: reqwest::Client,
    requests: Arc<Semaphore>,
}

impl Embedding {
    pub fn new(config: EmbeddingConfig) -> Result<Self> {
        config.validate()?;
        if config.enabled && config.api_key.trim().is_empty() {
            return Err(EmbeddingError::Configuration.into());
        }
        let endpoint = config.endpoint()?;
        let namespace = serde_json::to_vec(&(
            endpoint.as_str(),
            &config.model,
            config.dimensions,
            "float",
            TEMPLATE_VERSION,
        ))
        .map_err(|_| EmbeddingError::Configuration)?;
        let fingerprint = format!("sha256:{:x}", Sha256::digest(namespace));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(config.request_timeout_secs.min(5)))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|_| EmbeddingError::Configuration)?;
        Ok(Self {
            config,
            endpoint,
            fingerprint,
            http,
            requests: Arc::new(Semaphore::new(1)),
        })
    }

    /// Text and JPEG use the same model space; changing credentials does not.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn dimensions(&self) -> usize {
        self.config.dimensions
    }

    /// Revalidation is polled after the concurrency permit, immediately before
    /// HTTP submission. Callers must also revalidate before persisting results.
    pub async fn embed_text(
        &self,
        text: &str,
        cancel: &CancellationToken,
        revalidate: impl Future<Output = Result<()>> + Send,
    ) -> Result<EmbeddingResponse> {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(EmbeddingError::Input.into());
        }
        self.request(json!({"text":text}), cancel, revalidate).await
    }

    /// Only caller-supplied JPEG bytes are accepted; this module never fetches
    /// image URLs. The source collector owns decoding and metadata removal.
    pub async fn embed_jpeg(
        &self,
        jpeg: &[u8],
        cancel: &CancellationToken,
        revalidate: impl Future<Output = Result<()>> + Send,
    ) -> Result<EmbeddingResponse> {
        validate_jpeg(jpeg)?;
        let image = format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg));
        self.request(json!({"image":image}), cancel, revalidate)
            .await
    }

    async fn request(
        &self,
        input: Value,
        cancel: &CancellationToken,
        revalidate: impl Future<Output = Result<()>> + Send,
    ) -> Result<EmbeddingResponse> {
        if !self.config.enabled {
            return Err(EmbeddingError::Disabled.into());
        }
        // The deadline includes queueing, source checks, headers and body reads.
        let operation = async {
            let _permit = self
                .requests
                .acquire()
                .await
                .map_err(|_| EmbeddingError::SchedulerClosed)?;
            let body = json!({
                "model":self.config.model,
                "input":input,
                "encoding_format":"float",
                "dimensions":self.config.dimensions,
            });
            // Build before the source check so validation is the last async
            // step before send; provider/client errors never retain raw input.
            let request = self
                .http
                .post(self.endpoint.clone())
                .bearer_auth(&self.config.api_key)
                .json(&body)
                .build()
                .map_err(|_| EmbeddingError::Transport)?;
            revalidate
                .await
                .map_err(|_| EmbeddingError::SourceChanged)?;
            if cancel.is_cancelled() {
                return Err(EmbeddingError::Cancelled.into());
            }
            let mut response = self.http.execute(request).await.map_err(|error| {
                if error.is_timeout() {
                    EmbeddingError::Timeout
                } else {
                    EmbeddingError::Transport
                }
            })?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(EmbeddingError::HttpStatus(response.status().as_u16()).into());
            }
            if response
                .content_length()
                .is_some_and(|length| length > self.config.max_response_bytes as u64)
            {
                return Err(EmbeddingError::ResponseTooLarge.into());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| EmbeddingError::Transport)?
            {
                if chunk.len() > self.config.max_response_bytes.saturating_sub(bytes.len()) {
                    return Err(EmbeddingError::ResponseTooLarge.into());
                }
                bytes.extend_from_slice(&chunk);
            }
            parse_response(&bytes, &self.config)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(EmbeddingError::Cancelled.into()),
            result = tokio::time::timeout(Duration::from_secs(self.config.request_timeout_secs), operation) =>
                result.map_err(|_| EmbeddingError::Timeout)?,
        }
    }
}

fn validate_jpeg(jpeg: &[u8]) -> Result<()> {
    if jpeg.len() > MAX_JPEG_BYTES
        || !jpeg.starts_with(&[0xff, 0xd8, 0xff])
        || !jpeg.ends_with(&[0xff, 0xd9])
    {
        return Err(EmbeddingError::Input.into());
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| EmbeddingError::Input)?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 4 * 1024 * 1024 {
        return Err(EmbeddingError::Input.into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct WireResponse {
    object: String,
    model: String,
    data: Vec<WireEmbedding>,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireEmbedding {
    object: String,
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    total_tokens: u64,
    // SiliconFlow counts image input separately from prompt_tokens.
    #[serde(default)]
    image_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

fn parse_response(bytes: &[u8], config: &EmbeddingConfig) -> Result<EmbeddingResponse> {
    let mut response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| EmbeddingError::Response)?;
    // Ignore provider metadata extensions, but retain strict types/duplicates
    // for every consumed field and validate the complete vector below.
    let input_tokens = response
        .usage
        .prompt_tokens
        .checked_add(response.usage.image_tokens)
        .ok_or(EmbeddingError::Response)?;
    if response.object != "list"
        || response.model != config.model
        || response.data.len() != 1
        || response.usage.total_tokens < input_tokens
        || response.usage.completion_tokens != 0
    {
        return Err(EmbeddingError::Response.into());
    }
    let embedding = response.data.pop().ok_or(EmbeddingError::Response)?;
    if embedding.object != "embedding"
        || embedding.index != 0
        || embedding.embedding.len() != config.dimensions
    {
        return Err(EmbeddingError::Response.into());
    }
    let mut vector = embedding.embedding;
    normalize(&mut vector).map_err(|_| EmbeddingError::Response)?;
    Ok(EmbeddingResponse {
        vector,
        input_tokens: response.usage.total_tokens,
    })
}

fn norm(vector: &[f32]) -> Option<f64> {
    if !(64..=4096).contains(&vector.len()) || vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    // Every finite f32 square, and their bounded sum, fits comfortably in f64.
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    (norm > 0.0 && norm.is_finite()).then_some(norm)
}

pub fn normalize(vector: &mut [f32]) -> Result<()> {
    let norm = norm(vector).ok_or(EmbeddingError::Vector)?;
    for value in vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(())
}

/// Durable representation: finite, normalized little-endian f32, no headers.
pub fn encode_vector(vector: &[f32]) -> Result<Vec<u8>> {
    let norm = norm(vector).ok_or(EmbeddingError::Vector)?;
    Ok(vector
        .iter()
        .flat_map(|value| ((f64::from(*value) / norm) as f32).to_le_bytes())
        .collect())
}

pub fn decode_vector(bytes: &[u8], dimensions: usize) -> Result<Vec<f32>> {
    if !(64..=4096).contains(&dimensions) || bytes.len() != dimensions * 4 {
        return Err(EmbeddingError::Vector.into());
    }
    let mut vector: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    normalize(&mut vector)?;
    Ok(vector)
}

pub fn cosine(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() {
        return None;
    }
    let denominator = norm(left)? * norm(right)?;
    let dot: f64 = left
        .iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum();
    Some((dot / denominator).clamp(-1.0, 1.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, body::Body, extract::State, http::StatusCode, routing::post};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    fn config(url: String) -> EmbeddingConfig {
        EmbeddingConfig {
            enabled: true,
            embeddings_url: url,
            api_key: "synthetic-embedding-key".into(),
            dimensions: 64,
            ..EmbeddingConfig::default()
        }
    }

    fn wire() -> Value {
        let mut vector = vec![0.0; 64];
        vector[0] = 3.0;
        vector[1] = 4.0;
        json!({"object":"list","model":EmbeddingConfig::default().model,
            "data":[{"object":"embedding","index":0,"embedding":vector}],
            "usage":{"prompt_tokens":7,"total_tokens":7}})
    }

    fn jpeg() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        bytes.into_inner()
    }

    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}/embeddings"), task)
    }

    #[test]
    fn defaults_are_disabled_and_config_debug_and_errors_are_redacted() {
        let defaults: EmbeddingConfig = toml::from_str("").unwrap();
        assert!(!defaults.enabled);
        assert_eq!(defaults.dimensions, 1024);
        assert_eq!(defaults.request_timeout_secs, 15);
        defaults.validate().unwrap();
        let incomplete = EmbeddingConfig {
            enabled: true,
            ..defaults.clone()
        };
        incomplete.validate().unwrap();
        assert!(Embedding::new(incomplete).is_err());
        let cfg = config("https://synthetic-embedding-key@example.invalid/path?secret=yes".into());
        assert!(!format!("{cfg:?}").contains("synthetic-embedding-key"));
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            "embedding_invalid_configuration"
        );
        for dimensions in [0, 63, 4097, usize::MAX] {
            let cfg = EmbeddingConfig {
                dimensions,
                ..EmbeddingConfig::default()
            };
            assert!(cfg.validate().is_err());
        }
        for dimensions in [64, 1024, 4096] {
            EmbeddingConfig {
                dimensions,
                ..EmbeddingConfig::default()
            }
            .validate()
            .unwrap();
        }
        for endpoint in [
            "http://example.com/v1/embeddings",
            "file:///tmp/x",
            "https://x.test/?key=x",
            "https://x.test/#secret",
        ] {
            assert!(config(endpoint.into()).validate().is_err());
        }
        let mut cfg = config("http://127.0.0.1:1".into());
        cfg.api_key = "bad\nkey".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fingerprint_binds_space_but_not_credentials_or_request_limits() {
        let cfg = config("https://example.invalid/embeddings".into());
        let original = Embedding::new(cfg.clone()).unwrap();
        let mut rotated = cfg.clone();
        rotated.api_key = "rotated-synthetic-key".into();
        rotated.request_timeout_secs = 5;
        assert_eq!(
            original.fingerprint(),
            Embedding::new(rotated).unwrap().fingerprint()
        );
        for mutation in 0..3 {
            let mut changed = cfg.clone();
            match mutation {
                0 => changed.model = "another-model".into(),
                1 => changed.dimensions = 128,
                _ => changed.embeddings_url = "https://another.invalid/embeddings".into(),
            }
            assert_ne!(
                original.fingerprint(),
                Embedding::new(changed).unwrap().fingerprint()
            );
        }
        assert!(!original.fingerprint().contains(&cfg.api_key));
    }

    #[test]
    fn response_requires_one_exact_index_model_shape_and_finite_nonzero_dimensions() {
        let cfg = config("http://127.0.0.1:1".into());
        let response = parse_response(&serde_json::to_vec(&wire()).unwrap(), &cfg).unwrap();
        assert_eq!(response.input_tokens, 7);
        assert!((response.vector[0] - 0.6).abs() < 1e-6);
        assert!((response.vector[1] - 0.8).abs() < 1e-6);
        for mutation in 0..12 {
            let mut invalid = wire();
            match mutation {
                0 => invalid["object"] = json!("not-list"),
                1 => invalid["model"] = json!("different-space"),
                2 => invalid["data"] = json!([]),
                3 => invalid["data"]
                    .as_array_mut()
                    .unwrap()
                    .push(wire()["data"][0].clone()),
                4 => invalid["data"][0]["object"] = json!("wrong"),
                5 => invalid["data"][0]["index"] = json!(1),
                6 => invalid["data"][0]["embedding"] = json!(vec![1; 63]),
                7 => invalid["data"][0]["embedding"] = json!(vec![0; 64]),
                8 => invalid["data"][0]["embedding"][0] = json!("1.0"),
                9 => invalid["data"][0]["embedding"][0] = json!(1e100),
                10 => invalid["usage"]["total_tokens"] = json!(6),
                _ => invalid["usage"]["completion_tokens"] = json!(1),
            }
            assert!(
                parse_response(&serde_json::to_vec(&invalid).unwrap(), &cfg).is_err(),
                "mutation {mutation}"
            );
        }
        let duplicate = wire()
            .to_string()
            .replacen("\"index\":0", "\"index\":0,\"index\":0", 1);
        assert!(parse_response(duplicate.as_bytes(), &cfg).is_err());
        assert!(
            parse_response(br#"{"error":{"message":"secret-provider-content"}}"#, &cfg).is_err()
        );
    }

    #[test]
    fn siliconflow_text_and_image_usage_and_metadata_extensions_are_compatible() {
        let cfg = config("http://127.0.0.1:1".into());
        // Real synthetic responses on 2026-10-21: text includes image_tokens=0;
        // image usage separates its four image tokens from 18 prompt tokens.
        for (prompt, image, total) in [(21, 0, 21), (18, 4, 22)] {
            let mut value = wire();
            value["usage"] = json!({"prompt_tokens":prompt,"image_tokens":image,
                "total_tokens":total,"completion_tokens":0});
            let response = parse_response(&serde_json::to_vec(&value).unwrap(), &cfg).unwrap();
            assert_eq!(response.input_tokens, total);
            assert_eq!(response.vector.len(), cfg.dimensions);
            value["request_id"] = json!("synthetic-request-id");
            value["usage"]["billing_metadata"] = json!({"cached_tokens":0});
            value["data"][0]["metadata"] = json!({"synthetic":true});
            let extended = parse_response(&serde_json::to_vec(&value).unwrap(), &cfg).unwrap();
            assert_eq!(extended.input_tokens, total);
            assert_eq!(extended.vector, response.vector);
        }
        // Existing text-only compatible servers need not emit image_tokens.
        assert_eq!(
            parse_response(&serde_json::to_vec(&wire()).unwrap(), &cfg)
                .unwrap()
                .input_tokens,
            7
        );
    }

    #[test]
    fn usage_extensions_do_not_relax_known_fields_or_allow_invalid_token_counts() {
        let cfg = config("http://127.0.0.1:1".into());
        for invalid_usage in [
            json!({"prompt_tokens":18,"image_tokens":4,"total_tokens":21}),
            json!({"prompt_tokens":u64::MAX,"image_tokens":1,"total_tokens":u64::MAX}),
            json!({"prompt_tokens":18,"image_tokens":-1,"total_tokens":22}),
            json!({"prompt_tokens":18,"image_tokens":"4","total_tokens":22}),
            json!({"prompt_tokens":18,"image_tokens":null,"total_tokens":22}),
            json!({"prompt_tokens":18,"total_tokens":22,"completion_tokens":1}),
            json!({"prompt_tokens":18,"image_tokens":4}),
            json!({"image_tokens":4,"total_tokens":22}),
        ] {
            let mut value = wire();
            value["usage"] = invalid_usage;
            assert!(parse_response(&serde_json::to_vec(&value).unwrap(), &cfg).is_err());
        }
        for field in ["model", "prompt_tokens", "total_tokens", "index"] {
            let original = wire().to_string();
            let value = match field {
                "model" => json!(EmbeddingConfig::default().model).to_string(),
                "index" => "0".into(),
                _ => "7".into(),
            };
            let needle = format!("\"{field}\":{value}");
            let duplicate = original.replacen(&needle, &format!("{needle},{needle}"), 1);
            assert_ne!(duplicate, original);
            assert!(
                parse_response(duplicate.as_bytes(), &cfg).is_err(),
                "duplicate {field}"
            );
        }
    }

    #[test]
    fn vector_codec_rejects_corruption_and_cosine_is_stable_at_float_extremes() {
        let mut vector = vec![0.0; 64];
        vector[0] = f32::MAX;
        vector[1] = f32::MAX;
        let bytes = encode_vector(&vector).unwrap();
        assert_eq!(bytes.len(), 64 * 4);
        let decoded = decode_vector(&bytes, 64).unwrap();
        assert!((cosine(&vector, &decoded).unwrap() - 1.0).abs() < 1e-6);
        assert!(decode_vector(&bytes[..255], 64).is_err());
        assert!(decode_vector(&bytes, 128).is_err());
        assert!(decode_vector(&[0; 256], 64).is_err());
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            vector[0] = invalid;
            assert!(encode_vector(&vector).is_err());
            assert!(cosine(&vector, &decoded).is_none());
            let mut corrupt = bytes.clone();
            corrupt[..4].copy_from_slice(&invalid.to_le_bytes());
            assert!(decode_vector(&corrupt, 64).is_err());
        }
        let mut tiny = vec![f32::from_bits(1); 64];
        normalize(&mut tiny).unwrap();
        assert!((tiny[0] - 0.125).abs() < 1e-6);
        assert_eq!(
            cosine(&tiny, &tiny.iter().map(|n| -*n).collect::<Vec<_>>()),
            Some(-1.0)
        );
    }

    #[tokio::test]
    async fn sends_documented_text_and_inline_jpeg_with_request_local_auth() {
        type Received = Arc<Mutex<Vec<(String, Value)>>>;
        let received: Received = Arc::default();
        let (url, server) = serve(
            Router::new()
                .route(
                    "/embeddings",
                    post(
                        |State(received): State<Received>,
                         headers: axum::http::HeaderMap,
                         Json(body): Json<Value>| async move {
                            received
                                .lock()
                                .unwrap()
                                .push((headers["authorization"].to_str().unwrap().into(), body));
                            Json(wire())
                        },
                    ),
                )
                .with_state(received.clone()),
        )
        .await;
        let client = Embedding::new(config(url)).unwrap();
        let cancel = CancellationToken::new();
        client
            .embed_text("synthetic memory", &cancel, async { Ok(()) })
            .await
            .unwrap();
        let pixels = jpeg();
        client
            .embed_jpeg(&pixels, &cancel, async { Ok(()) })
            .await
            .unwrap();
        let requests = received.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (auth, body) in requests.iter() {
            assert_eq!(auth, "Bearer synthetic-embedding-key");
            assert_eq!(body["model"], EmbeddingConfig::default().model);
            assert_eq!(body["dimensions"], 64);
            assert_eq!(body["encoding_format"], "float");
            assert_eq!(body.as_object().unwrap().len(), 4);
        }
        assert_eq!(requests[0].1["input"], json!({"text":"synthetic memory"}));
        assert_eq!(
            requests[1].1["input"],
            json!({"image":format!("data:image/jpeg;base64,{}",STANDARD.encode(pixels))})
        );
        server.abort();
    }

    #[tokio::test]
    async fn rejects_unbounded_invalid_or_disabled_input_before_validation_callback() {
        let client = Embedding::new(config("http://127.0.0.1:1".into())).unwrap();
        let cancel = CancellationToken::new();
        for text in [String::new(), " \n ".into(), "x".repeat(MAX_TEXT_BYTES + 1)] {
            assert!(
                client
                    .embed_text(&text, &cancel, async {
                        panic!("invalid input reached validation")
                    })
                    .await
                    .is_err()
            );
        }
        for bytes in [
            vec![],
            b"https://example.invalid/image.jpg".to_vec(),
            vec![0xff, 0xd8, 0xff, 0xd9],
            vec![0; MAX_JPEG_BYTES + 1],
        ] {
            assert!(
                client
                    .embed_jpeg(&bytes, &cancel, async {
                        panic!("invalid JPEG reached validation")
                    })
                    .await
                    .is_err()
            );
        }
        let disabled = Embedding::new(EmbeddingConfig::default()).unwrap();
        let error = disabled
            .embed_text("synthetic", &cancel, async { panic!("disabled request") })
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<EmbeddingError>(),
            Some(&EmbeddingError::Disabled)
        );
    }

    #[tokio::test]
    async fn provider_errors_redirects_and_streamed_body_limits_do_not_leak_or_follow() {
        let redirects = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/embeddings",
                post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/sink")],
                        "secret-provider-content",
                    )
                }),
            )
            .route(
                "/sink",
                post(|State(count): State<Arc<AtomicUsize>>| async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(wire())
                }),
            )
            .with_state(redirects.clone());
        let (url, server) = serve(router).await;
        let client = Embedding::new(config(url)).unwrap();
        let error = client
            .embed_text("synthetic", &CancellationToken::new(), async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(format!("{error:#}"), "embedding_http_status_307");
        assert_eq!(redirects.load(Ordering::SeqCst), 0);
        server.abort();

        let (url, server) = serve(Router::new().route(
            "/embeddings",
            post(|| async {
                Body::from_stream(futures_util::stream::iter([
                    Ok::<_, std::io::Error>(vec![b'x'; 800]),
                    Ok(vec![b'x'; 800]),
                ]))
            }),
        ))
        .await;
        let mut cfg = config(url);
        cfg.max_response_bytes = 1024;
        let client = Embedding::new(cfg).unwrap();
        let error = client
            .embed_text("synthetic", &CancellationToken::new(), async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "embedding_response_too_large");
        server.abort();
    }

    #[tokio::test]
    async fn queued_sources_are_revalidated_after_permit_and_never_sent_if_revoked() {
        let hits = Arc::new(AtomicUsize::new(0));
        let (url, server) = serve(
            Router::new()
                .route(
                    "/embeddings",
                    post(|State(hits): State<Arc<AtomicUsize>>| async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(wire())
                    }),
                )
                .with_state(hits.clone()),
        )
        .await;
        let client = Embedding::new(config(url)).unwrap();
        let permit = client.requests.acquire().await.unwrap();
        let checked = AtomicBool::new(false);
        let still_valid = AtomicBool::new(true);
        let cancel = CancellationToken::new();
        let request = client.embed_text("forgotten synthetic memory", &cancel, async {
            checked.store(true, Ordering::SeqCst);
            anyhow::ensure!(
                still_valid.load(Ordering::SeqCst),
                "sensitive validation context"
            );
            Ok(())
        });
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut request)
                .await
                .is_err()
        );
        assert!(!checked.load(Ordering::SeqCst));
        still_valid.store(false, Ordering::SeqCst);
        drop(permit);
        let error = request.await.unwrap_err();
        assert_eq!(format!("{error:#}"), "embedding_source_changed");
        assert!(checked.load(Ordering::SeqCst));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(client.requests.available_permits(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn queue_deadline_and_cancellation_release_slots_without_running_source_check() {
        let mut cfg = config("http://127.0.0.1:1".into());
        cfg.request_timeout_secs = 1;
        let client = Embedding::new(cfg).unwrap();
        let permit = client.requests.acquire().await.unwrap();
        let cancel = CancellationToken::new();
        let error = client
            .embed_text("queued", &cancel, async {
                panic!("queue timeout ran check")
            })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "embedding_timeout");
        cancel.cancel();
        let error = client
            .embed_text("cancelled", &cancel, async {
                panic!("cancelled request ran check")
            })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "embedding_cancelled");
        drop(permit);
        assert_eq!(client.requests.available_permits(), 1);
    }
}
