# deepseek-mcp

A single-binary Rust MCP server for DeepSeek's official **Search** and **Fetch**
APIs, with a long-lived local daemon inside the same executable.

Two modes:

| Command | Role |
|---|---|
| `deepseek-mcp` (no args) | stdio MCP server launched by Codex |
| `deepseek-mcp --server` | long-lived loopback HTTP daemon (auto-started) |

The stdio side checks for the daemon via pid file + `/health`, starts it with
`--server` if missing, and proxies tool calls over loopback HTTP. The daemon owns
the DeepSeek HTTP client, keeps HTTPS connections warm, and caches successful
search/fetch results.

```text
Codex
  │ stdio (MCP)
  ▼
deepseek-mcp                 (no args)
  │ 1. read pid file
  │ 2. GET /health on 127.0.0.1:9317
  │ 3. spawn `deepseek-mcp --server --config ...`
  │ 4. proxy tool calls over loopback HTTP
  ▼
deepseek-mcp --server       (long-lived daemon)
  │ pooled HTTPS keep-alive
  ├── search/fetch cache
  ▼
api.deepseek.com
```

Tools:

- **`paid_search`** — paid fallback web search; use only when no default
  `web_search` is available.
- **`paid_fetch`** — paid fallback page reader; use only when no default web
  fetch/reader is available.

Both are billed by DeepSeek per successful request.

Built with the official Rust MCP SDK [`rmcp`](https://crates.io/crates/rmcp) 3.8.

## Build

Requires Rust 1.85+ (edition 2024; tested with 1.97).

```bash
cargo build --release
```

One binary:

```text
target/release/deepseek-mcp
```

### Windows

Native:

```powershell
cargo build --release
```

Cross-compiling from Linux:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

The `.exe` is under `target/x86_64-pc-windows-gnu/release/`.
The daemon is the same `.exe` launched with `--server`; no second binary is
needed.

## Configuration

Copy `deepseek-mcp.example.toml` and set your DeepSeek API key:

```toml
[deepseek]
# Recommended: an ACL-protected file outside the repo.
api_key_file = "C:/Users/<you>/.dsh/deepseek_api_key"
# Or use an environment variable instead:
# api_key_env = "DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com"
timeout_secs = 30
max_retries = 2

[server]
bind = "127.0.0.1"
port = 9317
startup_timeout_secs = 10

[cache]
search_ttl_secs = 300
fetch_ttl_secs = 3600
max_entries = 500
```

Key resolution order: `api_key`, `api_key_file`, the named `api_key_env`, then
`DEEPSEEK_API_KEY`. If no `api_key_file` is configured, the daemon also tries
`~/.dsh/deepseek_api_key` when that file exists. Codex must pass an environment
variable through to the MCP child if you use `api_key_env`; see `env_vars`
below. The key-file path avoids that dependency entirely and is the recommended
setup for Codex Desktop.

Config search order:

1. `--config <path>`
2. `DEEPSEEK_MCP_CONFIG`
3. `~/.dsh/search_mcp.toml`  ← recommended
4. `./deepseek-mcp.toml`
5. `<config-dir>/deepseek-mcp/config.toml`
6. `~/.deepseek-mcp.toml`

Keep the config private: on Unix `chmod 600`; on Windows restrict the ACL.

## Install

Recommended layout on Windows:

```text
C:\Users\<you>\.dsh\bin\deepseek-mcp.exe
C:\Users\<you>\.dsh\search_mcp.toml
```

On Linux:

```text
~/.dsh/bin/deepseek-mcp
~/.dsh/search_mcp.toml
```

Copy the release binary into `.dsh/bin`; copy
`deepseek-mcp.example.toml` to `.dsh/search_mcp.toml` and put the API key in
`C:\Users\<you>\.dsh\deepseek_api_key` (one line, no quotes). Restrict both
files to the current user and the Codex sandbox group:

```powershell
icacls "$env:USERPROFILE\.dsh\deepseek_api_key" /inheritance:r `
  /grant:r "$env:USERNAME:(R)" "CodexSandboxUsers:(R)" `
  "SYSTEM:(F)" "Administrators:(F)"
```

The front-end searches `.dsh/search_mcp.toml` by default, so Codex can also omit
`--config` entirely.

## Codex MCP configuration

```toml
[mcp_servers.deepseek_search]
command = 'C:\Users\<you>\.dsh\bin\deepseek-mcp.exe'
# env_vars = ["DEEPSEEK_API_KEY"]  # only needed for api_key_env
startup_timeout_sec = 30
# No args: stdio mode; config is found at ~/.dsh/search_mcp.toml.
```

No arguments means stdio mode. Codex does not pass `--server`; the front-end
starts the daemon lazily on the first paid tool call. With `api_key_file`, no
environment variable is needed. If you use `api_key_env`, add
`env_vars = ["DEEPSEEK_API_KEY"]` so Codex forwards it to the MCP child.

Manual daemon run for debugging:

```powershell
deepseek-mcp.exe --server --config C:\Users\<you>\.dsh\search_mcp.toml
```

## Lifecycle

With no `--server`, the binary **serves stdio MCP immediately**. It does not
start or wait for the daemon during `initialize`/`tools/list`; this avoids Codex
MCP startup timeouts and tool-discovery races.

On the first **paid** tool call (one with `confirm=true`):

1. Reads the daemon PID file and calls `GET /health`.
2. If healthy, reuses the daemon.
3. Otherwise takes an exclusive lock, removes a stale PID file, launches the
   **same executable** with `--server --config ...` detached, and waits for
   `/health` for up to `server.startup_timeout_secs`.
4. If the daemon cannot become healthy, returns a tool error with the log path.

PID, lock, and log files live under the stable `~/.dsh/run` directory, so Codex's
MSIX package redirection of `%LOCALAPPDATA%` cannot split the single-instance
lock. The daemon survives the stdio process, so the next Codex session reuses the
warm HTTPS pool and cache.

Default daemon files (port 9317):

```text
Windows: %USERPROFILE%\.dsh\run\server-9317.pid / .log / .lock
Linux:   ~/.dsh/run/server-9317.pid / .log / .lock
```

Override with `server.pid_file` / `server.log_file`.

### Restarting after config or binary changes

The daemon does not watch the config or binary. After changing the API key, base
URL, cache settings, or upgrading the executable, stop the daemon and let the
next MCP start it:

```powershell
# Windows
Stop-Process -Id (Get-Content "$env:USERPROFILE\.dsh\run\server-9317.pid")
```

```bash
# Linux
kill "$(cat ~/.dsh/run/server-9317.pid)"
```

A stale PID file is removed automatically on the next start. A version mismatch
is reported instead of silently reusing an old daemon.

## Internal HTTP API

The daemon only accepts loopback binds (`127.0.0.1` or `::1`); non-loopback
binds are rejected at config validation. There is no token auth. Access control
relies on the OS user boundary and the loopback interface.

The stdio front-end speaks MCP; this simple JSON API is internal:

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | liveness, pid, version, cache stats |
| `POST` | `/v1/search` | cached DeepSeek search |
| `POST` | `/v1/fetch` | cached DeepSeek fetch |

## Tools

Both tools are **paid fallbacks**. Prefer the client's default web search/fetch
when it exists; use these only when it does not. DeepSeek bills each successful
Search/Fetch request.

### `paid_search`

Use only when no default `web_search` tool is available. Every successful call
is billed by DeepSeek.

Parameters:

| Field | Type | Required | Notes |
|---|---|---|---|
| `query` | string | yes | 1–128 chars; supports `site:`, `lang:`, `after:`, `before:`, quotes, `-` exclusions |
| `count` | integer | no | 1–50, default 5 |
| `need_content` | boolean | no | Include page content; may be large |
| `ori_question` | string | no | Original user question, up to 128 chars |
| `localization` | string | no | Optional IP for localized results |
| `confirm` | boolean | yes | Must be `true`; acknowledges the billable call |
| `compact` | boolean | no | Default `true`: title/url/summary/date/authority/source_tier only; summary is capped at 500 chars |

Returns JSON with `query`, `count`, `need_content`, `untrusted: true`, a
`security_note`, and `results[]`. Compact results omit `snippet`, full `summary`,
and `content` unless `need_content=true`; use `compact=false` for all fields.
Results are deduplicated by URL and tagged `source_tier`:
`official`, `media`, `aggregator`, or `unknown`.

### `paid_fetch`

Use only when no default web fetch/reader is available. Every successful call
is billed by DeepSeek.

Parameters:

| Field | Type | Required | Notes |
|---|---|---|---|
| `url` | string | yes | HTTP/HTTPS, no username/password, max 8192 chars |
| `confirm` | boolean | yes | Must be `true`; acknowledges the billable call |
| `max_chars` | integer | no | Max Markdown characters returned, default 20000, clamped to 1–100000 |

Returns JSON with `url`, `title`, Markdown `content`, `untrusted: true`, and a
`security_note`. Truncated content ends with `[truncated]`.

Fetch is a reader, not a browser: no JavaScript rendering, login/paywall access,
or anti-bot bypass guarantees.

## Caching

The daemon caches only successful responses:

- Search: keyed by the full request, TTL `cache.search_ttl_secs`.
- Fetch: keyed by URL, TTL `cache.fetch_ttl_secs`.
- Max combined entries: `cache.max_entries`; expired entries are removed first,
  then the oldest entry is evicted.

`/health` exposes `entries`, `hits`, and `misses`.

## Behavior

- The daemon owns one `reqwest::Client` with pooled, keep-alive HTTPS.
- `timeout_secs` applies per DeepSeek request.
- `max_retries` retries 408, 429, 500, 502, 503, 504 with exponential backoff;
  `Retry-After` is honored.
- 400/401/402/403/404 are returned as tool errors with the upstream message.
- API keys are never logged.
- Logs go to stderr for stdio and to `server.log_file` for the daemon.
- Every daemon request logs `request_id`, `tool`, `duration_ms`, `cache_hit`,
  `result_count`, `http_status`, and `billed`; query text is not logged.
- A successful cached request logs `billed=false`; an upstream success logs
  `billed=true`. A failed request logs `billed=false` with the upstream status.
- Startup now serves stdio immediately; a missing key is reported on the first
  paid call instead of closing the `initialize` response.
- Set `RUST_LOG=debug` for more detail (the front-end passes its environment to
  the daemon when spawning it).

## Test

```bash
cargo test
```

Manual stdio handshake (one JSON object per line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"paid_search","arguments":{"query":"DeepSeek V4","count":3}}}
```

## DeepSeek API notes

- Search: `POST https://api.deepseek.com/v1/tools/search`
- Fetch: `POST https://api.deepseek.com/v1/tools/fetch`
- Billed per successful request; see the official docs for current pricing.
- Search QPS 100; Fetch QPS 25, plus per-domain Fetch limits.
- Docs: <https://api-docs.deepseek.com/guides/search>

## License

MIT
