//! Thin wrapper over `genai` for the Gemini provider. The actual agentic loop
//! (turn-by-turn calls with tool schemas) is built in `session.rs`; this module
//! exposes a configured `genai::Client` and the chosen model name.

use anyhow::{Context, Result, anyhow};
use genai::Client;
use genai::resolver::{AuthData, AuthResolver, Error as ResolverError};
use genai::{ModelIden};
use std::future::Future;
use std::pin::Pin;

use crate::config::Llm;

#[derive(Clone)]
pub struct LlmClient {
    pub client: Client,
    pub model: String,
}

impl LlmClient {
    pub fn from_config(cfg: &Llm) -> Result<Self> {
        if cfg.provider != "gemini" {
            return Err(anyhow!(
                "only `gemini` provider is wired up (got `{}`)",
                cfg.provider
            ));
        }
        let api_key = std::env::var("GEMINI_API_KEY")
            .context("GEMINI_API_KEY env var must be set")?;

        let client = build_gemini_client(api_key)?;
        Ok(Self { client, model: cfg.model.clone() })
    }
}

fn build_gemini_client(api_key: String) -> Result<Client> {
    let resolve = move |_model: ModelIden| -> Pin<
        Box<dyn Future<Output = Result<Option<AuthData>, ResolverError>> + Send + 'static>,
    > {
        let key = api_key.clone();
        Box::pin(async move { Ok(Some(AuthData::Key(key))) })
    };
    let resolver = AuthResolver::from_resolver_async_fn(resolve);
    Ok(Client::builder().with_auth_resolver(resolver).build())
}
