use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::protocol::{
    ErrorResponse, FetchRequest, FetchResponse, HealthResponse, SERVICE_NAME, SearchRequest,
    SearchResponse,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub fn new_request_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base_url: String,
}

impl DaemonClient {
    pub fn new(config: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(concat!("deepseek-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build daemon HTTP client")?;
        Ok(Self {
            http,
            base_url: config.http_base_url(),
        })
    }

    pub async fn health(&self) -> Result<HealthResponse, String> {
        let response = self
            .http
            .get(format!("{}/health", self.base_url))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("health returned HTTP {}", response.status()));
        }
        let health: HealthResponse = response.json().await.map_err(|e| e.to_string())?;
        if health.service != SERVICE_NAME {
            return Err(format!(
                "port is occupied by another service (service={:?})",
                health.service
            ));
        }
        Ok(health)
    }

    pub async fn search(
        &self,
        request_id: &str,
        request: &SearchRequest,
    ) -> Result<SearchResponse, String> {
        self.post("/v1/search", request_id, request).await
    }

    pub async fn fetch_page(&self, request_id: &str, url: &str) -> Result<FetchResponse, String> {
        let request = FetchRequest {
            url: url.to_string(),
        };
        self.post("/v1/fetch", request_id, &request).await
    }

    async fn post<T, B>(&self, path: &str, request_id: &str, body: &B) -> Result<T, String>
    where
        T: DeserializeOwned,
        B: Serialize,
    {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .header("x-request-id", request_id)
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if status.is_success() {
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())
        } else {
            let message = serde_json::from_slice::<ErrorResponse>(&bytes)
                .map(|error| error.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).chars().take(500).collect());
            Err(format!("daemon HTTP {status}: {message}"))
        }
    }
}

/// Ensure the long-lived daemon is running, starting it if necessary.
pub async fn ensure_daemon(config: &Config, config_path: &Path) -> Result<DaemonClient> {
    let client = DaemonClient::new(config)?;
    if let Ok(health) = client.health().await {
        ensure_version(&health)?;
        tracing::info!(url = %config.http_base_url(), "reusing existing deepseek-mcp-server");
        return Ok(client);
    }

    let lock_path = config.lock_file();
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;

    // Another MCP process may have started the daemon while we waited for the lock.
    if let Ok(health) = client.health().await {
        ensure_version(&health)?;
        tracing::info!(
            pid = health.pid,
            "reusing daemon started by another process"
        );
        return Ok(client);
    }

    let pid_path = config.pid_file();
    if let Ok(pid) = fs::read_to_string(&pid_path) {
        tracing::warn!(
            pid = pid.trim(),
            path = %pid_path.display(),
            "stale daemon pid file; removing"
        );
        let _ = fs::remove_file(&pid_path);
    }

    let binary = std::env::current_exe().context("failed to locate current executable")?;

    let log_path = config.log_file();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;
    writeln!(
        log,
        "\n=== starting {} at {} ===",
        binary.display(),
        chrono_like_now()
    )?;

    let absolute_config = fs::canonicalize(config_path)
        .with_context(|| format!("failed to resolve config path {}", config_path.display()))?;
    let mut command = Command::new(&binary);
    command
        .arg("--server")
        .arg("--config")
        .arg(&absolute_config)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {}", binary.display()))?;
    tracing::info!(pid = child.id(), binary = %binary.display(), "started deepseek-mcp-server");

    let deadline = Instant::now() + Duration::from_secs(config.server.startup_timeout_secs);
    loop {
        if let Ok(health) = client.health().await {
            ensure_version(&health)?;
            tracing::info!(
                pid = health.pid,
                uptime_secs = health.uptime_secs,
                "daemon is healthy"
            );
            return Ok(client);
        }

        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "deepseek-mcp-server exited during startup ({status}). Log: {}\n{}",
                log_path.display(),
                tail_log(&log_path, 2000)
            );
        }

        if Instant::now() >= deadline {
            bail!(
                "deepseek-mcp-server did not become healthy within {}s. Log: {}\n{}",
                config.server.startup_timeout_secs,
                log_path.display(),
                tail_log(&log_path, 2000)
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn ensure_version(health: &HealthResponse) -> Result<()> {
    if health.version != env!("CARGO_PKG_VERSION") {
        bail!(
            "deepseek-mcp-server v{} is already running (pid {}); stop it before using v{}",
            health.version,
            health.pid,
            env!("CARGO_PKG_VERSION")
        );
    }
    Ok(())
}

fn chrono_like_now() -> String {
    // Avoid a chrono dependency just for a log separator.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix={}", now.as_secs())
}

pub fn tail_log(path: &Path, max_chars: usize) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}
