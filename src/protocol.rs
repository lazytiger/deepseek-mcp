use serde::{Deserialize, Serialize};

pub const SERVICE_NAME: &str = "deepseek-mcp-server";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub count: u8,
    pub need_content: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ori_question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localization: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub rerank_score: Option<f64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub site_icon: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub authority: Option<i32>,
    /// official | media | aggregator | unknown, added by the daemon.
    #[serde(default)]
    pub source_tier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub cache: CacheStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
