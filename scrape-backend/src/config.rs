use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: Server,
    pub storage: Storage,
    pub llm: Llm,
    pub cors: Cors,
    #[serde(default)]
    pub routes: Vec<HostRoutes>,
    #[serde(default)]
    pub dashboard: Dashboard,
    #[serde(default)]
    pub sandbox: Sandbox,
}

/// Docker sandbox for file analysis (`run_command`) plus the per-session
/// workspace that uploads land in. See `crate::sandbox`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sandbox {
    /// Advertise `run_command` (still requires the image to be present).
    pub enabled: bool,
    pub image: String,
    /// Root for session workspaces (`sessions/<id>/`) and staged uploads.
    pub data_dir: String,
    /// `docker run --memory` / `--cpus` / `--pids-limit`.
    pub memory: String,
    pub cpus: String,
    pub pids_limit: u32,
    /// Default and maximum per-command timeout.
    pub command_timeout_secs: u64,
    pub max_command_timeout_secs: u64,
    /// stdout/stderr are each clipped to this many bytes in the tool result.
    pub max_output_bytes: usize,
    /// Remove a session's container after this long without a command.
    pub idle_ttl_secs: u64,
    /// Per-request cap on `POST /api/uploads`.
    pub max_upload_bytes: usize,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            enabled: true,
            image: "scrape-sandbox:0.1".into(),
            data_dir: "./data".into(),
            memory: "1g".into(),
            cpus: "1".into(),
            pids_limit: 512,
            command_timeout_secs: 30,
            max_command_timeout_secs: 120,
            max_output_bytes: 32 * 1024,
            idle_ttl_secs: 30 * 60,
            max_upload_bytes: 100 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dashboard {
    #[serde(default = "default_dist_path")]
    pub dist_path: String,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self { dist_path: default_dist_path() }
    }
}

fn default_dist_path() -> String {
    "./dashboard/build/client".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Storage {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Llm {
    /// Providers in priority order; see `crate::llm` for failover.
    #[serde(default)]
    pub providers: Vec<LlmProvider>,
    /// Legacy single-provider form, used only when `providers` is empty.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_refinements: u32,
    pub request_timeout_secs: u64,
    /// Extra attempts after a transient LLM failure (5xx, 429, network blip).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// First backoff delay; doubles each retry, capped at `retry_max_delay_ms`.
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
    #[serde(default = "default_retry_max_delay_ms")]
    pub retry_max_delay_ms: u64,
    /// After a provider fails over, skip it for this long so later steps go
    /// straight to the next one instead of waiting on it again.
    #[serde(default = "default_failover_cooldown_secs")]
    pub failover_cooldown_secs: u64,
}

/// One entry of `[[llm.providers]]`.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmProvider {
    /// gemini | anthropic | openai | meta | deepseek | mistral | openrouter |
    /// xai | groq | ollama | openai_compatible
    pub kind: String,
    pub model: String,
    /// Label for logs; defaults to `kind`.
    #[serde(default)]
    pub name: Option<String>,
    /// Env var holding the API key; defaults per kind (e.g. `ANTHROPIC_API_KEY`).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Override the endpoint (required for `openai_compatible`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// zero | minimal | low | medium | high | max — omit for the provider default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl Llm {
    /// The configured providers, or the legacy `provider`/`model` pair as one.
    pub fn provider_list(&self) -> Vec<LlmProvider> {
        if !self.providers.is_empty() {
            return self.providers.clone();
        }
        match (&self.provider, &self.model) {
            (Some(kind), Some(model)) => vec![LlmProvider {
                kind: kind.clone(),
                model: model.clone(),
                name: None,
                api_key_env: None,
                base_url: None,
                reasoning_effort: None,
            }],
            _ => Vec::new(),
        }
    }
}

fn default_max_retries() -> u32 { 3 }
fn default_retry_base_delay_ms() -> u64 { 1000 }
fn default_retry_max_delay_ms() -> u64 { 8000 }
fn default_failover_cooldown_secs() -> u64 { 300 }

#[derive(Debug, Clone, Deserialize)]
pub struct Cors {
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostRoutes {
    pub host: String,
    pub patterns: Vec<RoutePattern>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutePattern {
    #[serde(rename = "match")]
    pub match_re: String,
    pub replace: String,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("reading config at {}", path.as_ref().display()))?;
        let cfg: Config = toml::from_str(&contents).context("parsing config.toml")?;
        Ok(cfg)
    }
}
