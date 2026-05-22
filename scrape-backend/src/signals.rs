//! Incoming structural fingerprint from the extension. See SPEC §4.
//!
//! All fields are advisory — the LLM should probe via tools when something is
//! missing or marked `__truncated`. We deliberately keep the shape loose
//! (`serde_json::Value` for most nested fields) so the extension can evolve
//! its serializer without breaking this side.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signals {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub meta: Value,
    #[serde(default)]
    pub json_ld: Vec<Value>,
    #[serde(default)]
    pub next_data: Value,
    #[serde(default)]
    pub skeleton: Value,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub samples: Vec<Sample>,
    #[serde(default)]
    pub viewport: Option<Viewport>,
    #[serde(default)]
    pub extension_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub selector: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub selector: String,
    pub html: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// 256 KB cap, per SPEC §4.
pub const MAX_SIGNALS_BYTES: usize = 256 * 1024;
