use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use url::Url;

use crate::config::DeepSeekConfig;
use crate::protocol::{FetchResponse, SearchRequest, SearchResponse};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("DeepSeek API error (HTTP {status}): {message}")]
    Http { status: u16, message: String },
    #[error("DeepSeek API request failed: {0}")]
    Request(String),
    #[error("DeepSeek API returned invalid JSON: {0}")]
    Json(String),
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: Option<ErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    message: Option<String>,
}

/// Long-lived DeepSeek HTTP client. The daemon owns one instance so HTTPS
/// connections can be pooled and reused across many MCP tool calls.
#[derive(Clone)]
pub struct DeepSeekClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    max_retries: u32,
}

impl DeepSeekClient {
    pub fn new(config: &DeepSeekConfig, api_key: String) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .pool_max_idle_per_host(32)
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(concat!("deepseek-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ApiError::Request(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key,
            max_retries: config.max_retries,
        })
    }

    pub async fn search(&self, params: &SearchRequest) -> Result<SearchResponse, ApiError> {
        self.post_json("/v1/tools/search", params).await
    }

    pub async fn fetch_page(&self, url: &str) -> Result<FetchResponse, ApiError> {
        #[derive(Serialize)]
        struct Body<'a> {
            url: &'a str,
        }
        self.post_json("/v1/tools/fetch", &Body { url }).await
    }

    async fn post_json<T, B>(&self, path: &str, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        let url = format!("{}{path}", self.base_url);
        let mut attempt = 0u32;

        loop {
            let response = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await
                .map_err(|e| ApiError::Request(e.to_string()))?;

            let status = response.status();
            if status.is_success() {
                return response
                    .json::<T>()
                    .await
                    .map_err(|e| ApiError::Json(e.to_string()));
            }

            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            let body_text = response.text().await.unwrap_or_default();
            let message = extract_error_message(&body_text);

            if is_retryable(status) && attempt < self.max_retries {
                let backoff = Duration::from_millis(300 * 2u64.pow(attempt.min(4)));
                let delay = retry_after
                    .map(Duration::from_secs)
                    .unwrap_or(backoff)
                    .min(Duration::from_secs(5));
                tracing::warn!(
                    status = status.as_u16(),
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis(),
                    "DeepSeek API request failed; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }

            return Err(ApiError::Http {
                status: status.as_u16(),
                message,
            });
        }
    }
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn extract_error_message(body: &str) -> String {
    if let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(body) {
        if let Some(message) = envelope.error.and_then(|e| e.message) {
            return message;
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty response body".to_string()
    } else {
        trimmed.chars().take(500).collect()
    }
}

/// Validate a Fetch API URL according to DeepSeek's documented constraints.
pub fn validate_fetch_url(raw: &str) -> Result<(), String> {
    if raw.chars().count() > 8192 {
        return Err("url must be at most 8192 characters".to_string());
    }
    let url = Url::parse(raw).map_err(|e| format!("invalid url: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("url must use http or https".to_string());
    }
    if url.host_str().is_none() {
        return Err("url must contain a host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("url must not contain a username or password".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_fetch_urls() {
        assert!(validate_fetch_url("https://example.com/a").is_ok());
        assert!(validate_fetch_url("http://example.com").is_ok());
        assert!(validate_fetch_url("ftp://example.com").is_err());
        assert!(validate_fetch_url("https://user:pass@example.com").is_err());
        assert!(validate_fetch_url("not-a-url").is_err());
    }

    #[test]
    fn extracts_nested_error_message() {
        let body = r#"{"error":{"message":"'count' must be between 1 and 50"}}"#;
        assert_eq!(
            extract_error_message(body),
            "'count' must be between 1 and 50"
        );
    }
}
