use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub deepseek: DeepSeekConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeepSeekConfig {
    /// API key written directly in the config file.
    pub api_key: Option<String>,
    /// Alternatively, the name of an environment variable that holds the key.
    pub api_key_env: Option<String>,
    /// File containing the API key. Defaults to ~/.dsh/deepseek_api_key if present.
    pub api_key_file: Option<PathBuf>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

/// Settings for the local long-lived `deepseek-mcp-server` daemon.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address the daemon binds to. Must be a loopback address (127.0.0.1 or ::1).
    #[serde(default = "default_server_bind")]
    pub bind: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
    /// Optional PID file path. Defaults to the user cache directory.
    pub pid_file: Option<PathBuf>,
    /// Optional daemon log file path. Defaults to the user cache directory.
    pub log_file: Option<PathBuf>,
    /// How long the stdio MCP server waits for the daemon to become healthy.
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_server_bind(),
            port: default_server_port(),
            pid_file: None,
            log_file: None,
            startup_timeout_secs: default_startup_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_search_ttl_secs")]
    pub search_ttl_secs: u64,
    #[serde(default = "default_fetch_ttl_secs")]
    pub fetch_ttl_secs: u64,
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            search_ttl_secs: default_search_ttl_secs(),
            fetch_ttl_secs: default_fetch_ttl_secs(),
            max_entries: default_max_entries(),
        }
    }
}

fn default_base_url() -> String {
    "https://api.deepseek.com".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_max_retries() -> u32 {
    2
}

fn default_server_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_server_port() -> u16 {
    9317
}

fn default_startup_timeout_secs() -> u64 {
    10
}

fn default_search_ttl_secs() -> u64 {
    300
}

fn default_fetch_ttl_secs() -> u64 {
    3600
}

fn default_max_entries() -> usize {
    500
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.deepseek.timeout_secs == 0 {
            bail!("deepseek.timeout_secs must be greater than 0");
        }
        if self.deepseek.max_retries > 5 {
            bail!("deepseek.max_retries must be 5 or less");
        }

        let base = self.deepseek.base_url.trim_end_matches('/');
        let url = Url::parse(base).context("deepseek.base_url is not a valid URL")?;
        if url.scheme() != "https" && url.scheme() != "http" {
            bail!("deepseek.base_url must use http or https");
        }
        if url.host_str().is_none() {
            bail!("deepseek.base_url must contain a host");
        }

        if self.server.port == 0 {
            bail!("server.port must be greater than 0");
        }
        if self.server.startup_timeout_secs == 0 {
            bail!("server.startup_timeout_secs must be greater than 0");
        }
        let bind: IpAddr = self
            .server
            .bind
            .parse()
            .context("server.bind must be an IP address such as 127.0.0.1")?;
        if !bind.is_loopback() {
            bail!(
                "server.bind must be a loopback address (127.0.0.1 or ::1); \
                 this daemon does not expose a non-loopback listener"
            );
        }

        if self.cache.search_ttl_secs == 0 || self.cache.fetch_ttl_secs == 0 {
            bail!("cache TTL values must be greater than 0");
        }
        if self.cache.max_entries == 0 {
            bail!("cache.max_entries must be greater than 0");
        }
        Ok(())
    }

    pub fn base_url(&self) -> String {
        self.deepseek.base_url.trim_end_matches('/').to_string()
    }

    /// Resolve the API key from config, key file, or environment.
    pub fn resolve_api_key(&self) -> Result<String> {
        let direct = self
            .deepseek
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);

        let configured_file = self.deepseek.api_key_file.clone();
        let default_file = dirs::home_dir()
            .map(|home| home.join(".dsh").join("deepseek_api_key"))
            .filter(|path| path.is_file());
        let file_key = configured_file
            .and_then(|path| fs::read_to_string(path).ok())
            .or_else(|| default_file.and_then(|path| fs::read_to_string(path).ok()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let env_name = self
            .deepseek
            .api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let env_key = env_name
            .and_then(|name| env::var(name).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                env::var("DEEPSEEK_API_KEY")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            });

        let key = direct.or(file_key).or(env_key).context(
            "DeepSeek API key not found: set deepseek.api_key, deepseek.api_key_file, \
             deepseek.api_key_env, or DEEPSEEK_API_KEY",
        )?;
        if !key.starts_with("sk-") {
            bail!("DeepSeek API key should start with \"sk-\"");
        }
        Ok(key)
    }

    pub fn pid_file(&self) -> PathBuf {
        self.server
            .pid_file
            .clone()
            .unwrap_or_else(|| state_dir().join(format!("server-{}.pid", self.server.port)))
    }

    pub fn log_file(&self) -> PathBuf {
        self.server
            .log_file
            .clone()
            .unwrap_or_else(|| state_dir().join(format!("server-{}.log", self.server.port)))
    }

    pub fn lock_file(&self) -> PathBuf {
        let pid = self.pid_file();
        pid.with_extension("lock")
    }

    pub fn http_base_url(&self) -> String {
        let bind: IpAddr = self
            .server
            .bind
            .parse()
            .unwrap_or(IpAddr::from([127, 0, 0, 1]));
        let host = if bind.is_unspecified() {
            "127.0.0.1".to_string()
        } else if bind.is_ipv6() {
            format!("[{bind}]")
        } else {
            bind.to_string()
        };
        format!("http://{host}:{}", self.server.port)
    }
}

fn state_dir() -> PathBuf {
    // Keep runtime state next to the .dsh config instead of using
    // dirs::cache_dir(): Codex's MSIX package redirects LocalAppData, so
    // different front-ends could otherwise use different lock/PID files.
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dsh")
        .join("run")
}

pub fn config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".dsh").join("search_mcp.toml"));
    }
    paths.push(PathBuf::from("deepseek-mcp.toml"));
    if let Some(dir) = dirs::config_dir() {
        paths.push(dir.join("deepseek-mcp").join("config.toml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".deepseek-mcp.toml"));
    }
    paths
}

pub fn find_config(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("config file not found: {}", path.display());
        }
        return Ok(path.to_path_buf());
    }

    if let Ok(path) = env::var("DEEPSEEK_MCP_CONFIG") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "DEEPSEEK_MCP_CONFIG points to a missing file: {}",
            path.display()
        );
    }

    for path in config_search_paths() {
        if path.is_file() {
            return Ok(path);
        }
    }

    let tried = config_search_paths()
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "no config file found. Create one of:\n{tried}\n\
         or pass --config <path>. See deepseek-mcp.example.toml."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn parses_minimal_config() {
        let (_dir, path) = write_config(
            r#"
[deepseek]
api_key = "sk-test"
"#,
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(config.base_url(), "https://api.deepseek.com");
        assert_eq!(config.deepseek.timeout_secs, 30);
        assert_eq!(config.server.port, 9317);
        assert_eq!(config.cache.search_ttl_secs, 300);
        assert_eq!(config.resolve_api_key().unwrap(), "sk-test");
    }

    #[test]
    fn rejects_bad_base_url() {
        let (_dir, path) = write_config(
            r#"
[deepseek]
api_key = "sk-test"
base_url = "not a url"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn rejects_non_sk_key() {
        let (_dir, path) = write_config(
            r#"
[deepseek]
api_key = "Bearer sk-test"
"#,
        );
        let config = Config::load(&path).unwrap();
        assert!(config.resolve_api_key().is_err());
    }

    #[test]
    fn allows_env_key_name() {
        let (_dir, path) = write_config(
            r#"
[deepseek]
api_key_env = "MY_TEST_DEEPSEEK_KEY"
"#,
        );
        // SAFETY: test-only, unique variable name.
        unsafe { env::set_var("MY_TEST_DEEPSEEK_KEY", "sk-from-env") };
        let config = Config::load(&path).unwrap();
        assert_eq!(config.resolve_api_key().unwrap(), "sk-from-env");
        unsafe { env::remove_var("MY_TEST_DEEPSEEK_KEY") };
    }

    #[test]
    fn rejects_non_loopback_daemon_bind() {
        let (_dir, path) = write_config(
            r#"
[deepseek]
api_key = "sk-test"

[server]
bind = "0.0.0.0"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn allows_ipv6_loopback_daemon_bind() {
        let (_dir, path) = write_config(
            r#"
[deepseek]
api_key = "sk-test"

[server]
bind = "::1"
"#,
        );
        assert!(Config::load(&path).is_ok());
    }
}
