use std::path::PathBuf;

use anyhow::{Result, bail};
use deepseek_mcp::config::{Config, find_config};
use deepseek_mcp::daemon;
use deepseek_mcp::daemon_client::{DaemonClient, ensure_daemon};
use deepseek_mcp::deepseek::validate_fetch_url;
use deepseek_mcp::protocol::{FetchResponse, SearchRequest, SearchResponse};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct DeepSeekServer {
    daemon: DaemonClient,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchToolParams {
    /// Search query (1-128 characters). Supports DeepSeek operators such as
    /// site:, lang:, after:, before:, quoted phrases and -exclusions.
    query: String,
    /// Number of results, 1-50. Defaults to 10.
    #[schemars(range(min = 1, max = 50))]
    count: Option<u8>,
    /// Return page content in each result. May be large.
    need_content: Option<bool>,
    /// The user's original question, used to improve query understanding.
    ori_question: Option<String>,
    /// Optional IP address used to localize results.
    localization: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FetchToolParams {
    /// HTTP/HTTPS URL to fetch. The page is returned as Markdown.
    url: String,
}

#[tool_router]
impl DeepSeekServer {
    #[tool(
        name = "paid_search",
        description = "PAID fallback web search via DeepSeek. Only use this when the \
                       environment has no default/built-in web_search tool. Every \
                       successful search request is billed by DeepSeek."
    )]
    async fn paid_search(
        &self,
        Parameters(params): Parameters<SearchToolParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = params.query.trim().to_string();
        if query.is_empty() {
            return Ok(tool_error("query must not be empty"));
        }
        if query.chars().count() > 128 {
            return Ok(tool_error("query must be at most 128 characters"));
        }

        let count = params.count.unwrap_or(10);
        if !(1..=50).contains(&count) {
            return Ok(tool_error("count must be between 1 and 50"));
        }

        let ori_question = match params.ori_question {
            Some(value) => {
                let value = value.trim().to_string();
                if value.chars().count() > 128 {
                    return Ok(tool_error("ori_question must be at most 128 characters"));
                }
                (!value.is_empty()).then_some(value)
            }
            None => None,
        };

        let localization = params
            .localization
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        tracing::info!(
            tool = "paid_search",
            query_chars = query.chars().count(),
            count,
            need_content = params.need_content.unwrap_or(false),
            "tool call"
        );

        let request = SearchRequest {
            query,
            count,
            need_content: params.need_content.unwrap_or(false),
            ori_question,
            localization,
        };

        match self.daemon.search(&request).await {
            Ok(response) => search_result(request, response),
            Err(error) => Ok(tool_error(format!("DeepSeek search failed: {error}"))),
        }
    }

    #[tool(
        name = "paid_fetch",
        description = "PAID fallback page fetch via DeepSeek. Only use this when the \
                       environment has no default/built-in web fetch or web reader. \
                       Every successful fetch request is billed by DeepSeek."
    )]
    async fn paid_fetch(
        &self,
        Parameters(params): Parameters<FetchToolParams>,
    ) -> Result<CallToolResult, McpError> {
        let url = params.url.trim().to_string();
        if let Err(message) = validate_fetch_url(&url) {
            return Ok(tool_error(message));
        }

        tracing::info!(
            tool = "paid_fetch",
            url_chars = url.chars().count(),
            "tool call"
        );

        match self.daemon.fetch_page(&url).await {
            Ok(response) => fetch_result(url, response),
            Err(error) => Ok(tool_error(format!("DeepSeek fetch failed: {error}"))),
        }
    }
}

#[tool_handler(
    name = "deepseek-search",
    version = "0.2.0",
    instructions = "PAID fallback tools for DeepSeek Search & Fetch. Use paid_search \
                    only when no default web_search is available, and paid_fetch only when \
                    no default web fetch/reader is available. Successful calls incur fees."
)]
impl ServerHandler for DeepSeekServer {}

fn search_result(
    request: SearchRequest,
    response: SearchResponse,
) -> Result<CallToolResult, McpError> {
    let payload = json!({
        "query": request.query,
        "count": request.count,
        "need_content": request.need_content,
        "results": response.results,
    });
    json_result(payload)
}

fn fetch_result(url: String, response: FetchResponse) -> Result<CallToolResult, McpError> {
    let payload = json!({
        "url": url,
        "title": response.title,
        "content": response.content,
    });
    json_result(payload)
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn json_result(payload: serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|e| McpError::internal_error(format!("failed to serialize result: {e}"), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    server: bool,
    help: bool,
    version: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => args.server = true,
            "-h" | "--help" => args.help = true,
            "-V" | "--version" => args.version = true,
            "-c" | "--config" => {
                let path = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?;
                args.config = Some(PathBuf::from(path));
            }
            other if other.starts_with("--config=") => {
                args.config = Some(PathBuf::from(&other["--config=".len()..]));
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        r#"deepseek-mcp {}

Single-binary DeepSeek MCP server.
No arguments: stdio MCP front-end (used by Codex), which checks for a
loopback daemon, starts it if needed, and proxies tool calls.
--server: run that long-lived daemon.

USAGE:
    deepseek-mcp                 # stdio MCP
    deepseek-mcp --server        # long-lived daemon
    deepseek-mcp [--config <path>]

OPTIONS:
    --server             Run the long-lived HTTP daemon
    -c, --config <path>  Path to config.toml
    -h, --help           Print help
    -V, --version        Print version

CONFIG SEARCH ORDER:
    1. --config <path>
    2. DEEPSEEK_MCP_CONFIG
    3. ~/.dsh/search_mcp.toml
    4. ./deepseek-mcp.toml
    5. <config-dir>/deepseek-mcp/config.toml
    6. ~/.deepseek-mcp.toml"#,
        env!("CARGO_PKG_VERSION")
    );
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = parse_args()?;
    if args.help {
        print_help();
        return Ok(());
    }
    if args.version {
        println!("deepseek-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let config_path = find_config(args.config.as_deref())?;
    let config = Config::load(&config_path)?;

    if args.server {
        return daemon::run(config, config_path).await;
    }

    let daemon = ensure_daemon(&config, &config_path).await?;

    tracing::info!(
        daemon = %config.http_base_url(),
        "stdio MCP ready; tool calls go through the long-lived daemon"
    );

    let service = DeepSeekServer { daemon }
        .serve(stdio())
        .await
        .inspect_err(|error| tracing::error!(?error, "MCP server failed to start"))?;

    service.waiting().await?;
    Ok(())
}
