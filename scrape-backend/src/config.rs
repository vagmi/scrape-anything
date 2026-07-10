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
    pub provider: String,
    pub model: String,
    pub max_turns: u32,
    pub max_refinements: u32,
    pub request_timeout_secs: u64,
}

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
