use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::net::TcpListener;

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
        cache_entries = config.cache.max_entries,
        "deepseek-mcp-server ready"
    );

    // Do not install a ctrl-c handler here: the daemon is normally launched
    // detached and killed by PID. A failed console handler on Windows would
    // otherwise resolve the shutdown future immediately and stop the daemon.
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
    Json(request): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ErrorResponse>)> {
    if request.query.trim().is_empty() || request.query.chars().count() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "query must be 1-128 characters".to_string(),
            }),
        ));
    }
    if !(1..=50).contains(&request.count) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "count must be between 1 and 50".to_string(),
            }),
        ));
    }

    if let Some(cached) = state.cache.get_search(&request) {
        tracing::debug!(query = %request.query, "search cache hit");
        return Ok(Json(cached));
    }

    let response = state.client.search(&request).await.map_err(map_api_error)?;
    state.cache.insert_search(&request, response.clone());
    Ok(Json(response))
}

async fn fetch_page(
    State(state): State<AppState>,
    Json(request): Json<FetchRequest>,
) -> Result<Json<FetchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let url = request.url.trim().to_string();
    if let Err(message) = validate_fetch_url(&url) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: message }),
        ));
    }

    if let Some(cached) = state.cache.get_fetch(&url) {
        tracing::debug!(%url, "fetch cache hit");
        return Ok(Json(cached));
    }

    let response = state.client.fetch_page(&url).await.map_err(map_api_error)?;
    state.cache.insert_fetch(&url, response.clone());
    Ok(Json(response))
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
