use std::sync::Arc;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub config: Config,
    pub db: SqlitePool,
    pub llm: LlmClient,
    pub sandbox: Arc<Sandbox>,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool, llm: LlmClient, sandbox: Arc<Sandbox>) -> Self {
        Self { inner: Arc::new(Inner { config, db, llm, sandbox }) }
    }

    pub fn config(&self) -> &Config { &self.inner.config }
    pub fn db(&self) -> &SqlitePool { &self.inner.db }
    #[allow(dead_code)]
    pub fn llm(&self) -> &LlmClient { &self.inner.llm }
    pub fn sandbox(&self) -> &Sandbox { &self.inner.sandbox }
}
