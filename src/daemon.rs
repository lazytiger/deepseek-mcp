use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::net::TcpListener;
use url::Url;

use crate::cache::Cache;
use crate::config::Config;
use crate::deepseek::{ApiError, DeepSeekClient, validate_fetch_url};
use crate::protocol::{
    CacheStats, ErrorResponse, FetchRequest, FetchResponse, HealthResponse, SERVICE_NAME,
    SearchRequest, SearchResponse,
};

#[derive(Clone)]
struct AppState {
    client: DeepSeekClient,
    cache: Arc<Cache>,
    started: Instant,
    pid: u32,
}

struct PidGuard {
    path: PathBuf,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn run(config: Config, _config_path: PathBuf) -> Result<()> {
    let api_key = config.resolve_api_key()?;
    let client = DeepSeekClient::new(&config.deepseek, api_key)
        .map_err(|e| anyhow::anyhow!("failed to create DeepSeek client: {e}"))?;
    let cache = Arc::new(Cache::new(
        Duration::from_secs(config.cache.search_ttl_secs),
        Duration::from_secs(config.cache.fetch_ttl_secs),
        config.cache.max_entries,
    ));

    let addr: SocketAddr = format!("{}:{}", config.server.bind, config.server.port)
        .parse()
        .context("invalid server bind address")?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind deepseek-mcp-server on {addr}"))?;
    let addr = listener.local_addr()?;

    write_pid_file(&config.pid_file())?;
    let _pid_guard = PidGuard {
        path: config.pid_file(),
    };

    let state = AppState {
        client,
        cache,
        started: Instant::now(),
        pid: std::process::id(),
    };

    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/search", post(search))
        .route("/v1/fetch", post(fetch_page))
        .with_state(state);

    tracing::info!(
        %addr,
        pid = std::process::id(),
        pid_file = %config.pid_file().display(),
        log_file = %config.log_file().display(),
        cache_entries = config.cache.max_entries,
        "deepseek-mcp-server ready"
    );

    axum::serve(listener, router)
        .await
        .context("deepseek-mcp-server stopped")?;

    Ok(())
}

fn write_pid_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, std::process::id().to_string())
        .with_context(|| format!("failed to write {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("failed to rename {} to {}", temp.display(), path.display()))?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let CacheStats {
        entries,
        hits,
        misses,
    } = state.cache.stats();
    Json(HealthResponse {
        status: "ok".to_string(),
        service: SERVICE_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: state.pid,
        uptime_secs: state.started.elapsed().as_secs(),
        cache: CacheStats {
            entries,
            hits,
            misses,
        },
    })
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let request_id = request_id(&headers);
    let started = Instant::now();

    if request.query.trim().is_empty() || request.query.chars().count() > 128 {
        log_request(
            &request_id,
            "paid_search",
            started.elapsed(),
            false,
            0,
            StatusCode::BAD_REQUEST,
            false,
            Some("query must be 1-128 characters"),
        );
        return Err(bad_request("query must be 1-128 characters"));
    }
    if !(1..=50).contains(&request.count) {
        log_request(
            &request_id,
            "paid_search",
            started.elapsed(),
            false,
            0,
            StatusCode::BAD_REQUEST,
            false,
            Some("count must be between 1 and 50"),
        );
        return Err(bad_request("count must be between 1 and 50"));
    }

    if let Some(cached) = state.cache.get_search(&request) {
        log_request(
            &request_id,
            "paid_search",
            started.elapsed(),
            true,
            cached.results.len(),
            StatusCode::OK,
            false,
            None,
        );
        return Ok(Json(cached));
    }

    match state.client.search(&request).await {
        Ok(mut response) => {
            enrich_results(&mut response);
            state.cache.insert_search(&request, response.clone());
            log_request(
                &request_id,
                "paid_search",
                started.elapsed(),
                false,
                response.results.len(),
                StatusCode::OK,
                true,
                None,
            );
            Ok(Json(response))
        }
        Err(error) => {
            let (status, body) = map_api_error(error);
            log_request(
                &request_id,
                "paid_search",
                started.elapsed(),
                false,
                0,
                status,
                false,
                Some(&body.0.error),
            );
            Err((status, body))
        }
    }
}

async fn fetch_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<FetchRequest>,
) -> Result<Json<FetchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let request_id = request_id(&headers);
    let started = Instant::now();
    let url = request.url.trim().to_string();

    if let Err(message) = validate_fetch_url(&url) {
        log_request(
            &request_id,
            "paid_fetch",
            started.elapsed(),
            false,
            0,
            StatusCode::BAD_REQUEST,
            false,
            Some(&message),
        );
        return Err(bad_request(message));
    }

    if let Some(cached) = state.cache.get_fetch(&url) {
        log_request(
            &request_id,
            "paid_fetch",
            started.elapsed(),
            true,
            1,
            StatusCode::OK,
            false,
            None,
        );
        return Ok(Json(cached));
    }

    match state.client.fetch_page(&url).await {
        Ok(response) => {
            state.cache.insert_fetch(&url, response.clone());
            log_request(
                &request_id,
                "paid_fetch",
                started.elapsed(),
                false,
                1,
                StatusCode::OK,
                true,
                None,
            );
            Ok(Json(response))
        }
        Err(error) => {
            let (status, body) = map_api_error(error);
            log_request(
                &request_id,
                "paid_fetch",
                started.elapsed(),
                false,
                0,
                status,
                false,
                Some(&body.0.error),
            );
            Err((status, body))
        }
    }
}

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn log_request(
    request_id: &str,
    tool: &str,
    duration: Duration,
    cache_hit: bool,
    result_count: usize,
    http_status: StatusCode,
    billed: bool,
    error: Option<&str>,
) {
    tracing::info!(
        request_id,
        tool,
        duration_ms = duration.as_millis() as u64,
        cache_hit,
        result_count,
        http_status = http_status.as_u16(),
        billed,
        error = error.unwrap_or(""),
        "paid tool request"
    );
}

fn map_api_error(error: ApiError) -> (StatusCode, Json<ErrorResponse>) {
    let status = match &error {
        ApiError::Http { status, .. } => {
            StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        ApiError::Request(_) => StatusCode::BAD_GATEWAY,
        ApiError::Json(_) => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(ErrorResponse {
            error: error.to_string(),
        }),
    )
}

fn enrich_results(response: &mut SearchResponse) {
    for result in &mut response.results {
        result.source_tier = source_tier(&result.url);
    }

    let mut seen = HashSet::new();
    response.results.retain(|result| {
        let key = if result.url.trim().is_empty() {
            format!("title:{}", result.title.trim().to_lowercase())
        } else {
            normalize_url(&result.url)
        };
        seen.insert(key)
    });
}

fn normalize_url(raw: &str) -> String {
    let Ok(url) = Url::parse(raw) else {
        return raw.trim().to_lowercase();
    };
    let host = url.host_str().unwrap_or("").trim_start_matches("www.");
    let path = url.path().trim_end_matches('/');
    format!("{host}{path}")
}

fn source_tier(raw: &str) -> String {
    let Ok(url) = Url::parse(raw) else {
        return "unknown".to_string();
    };
    let host = url.host_str().unwrap_or("").to_lowercase();
    let host = host.trim_start_matches("www.");
    const OFFICIAL: &[&str] = &[
        "steampowered.com",
        "store.steampowered.com",
        "metacritic.com",
        "opencritic.com",
        "playstation.com",
        "sony.com",
        "s-game.com",
        "sgame.com",
        "guildos.ai",
    ];
    const MEDIA: &[&str] = &[
        "ign.com",
        "gamespot.com",
        "eurogamer.net",
        "pcgamer.com",
        "gamersky.com",
        "vg247.com",
        "polygon.com",
    ];
    const AGGREGATOR: &[&str] = &[
        "17173.com",
        "sohu.com",
        "360.cn",
        "9game.cn",
        "163.com",
        "qq.com",
        "sina.com.cn",
        "zhihu.com",
        "bilibili.com",
    ];

    if OFFICIAL
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        "official".to_string()
    } else if MEDIA
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        "media".to_string()
    } else if AGGREGATOR
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        "aggregator".to_string()
    } else {
        "unknown".to_string()
    }
}
