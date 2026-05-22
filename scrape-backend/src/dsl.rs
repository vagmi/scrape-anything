//! Plan DSL — the only structured output the LLM is allowed to emit. See
//! SPEC §7. We keep this strict so the extension's interpreter has a small,
//! auditable surface.
//!
//! Validation strategy: types here derive `Deserialize` with `#[serde(deny_unknown_fields)]`,
//! so anything off-grammar fails parsing. For producing a JSON Schema we hand-roll
//! the schema string below; that schema is also what's sent to the LLM as the
//! response schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub plan_id: String,
    pub kind: PlanKind,
    pub source: PlanSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<ItemsLocator>,
    pub fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_processors: Vec<Op>,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind { Listing, Event }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource { NextData, JsonLd, Dom }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ItemsLocator {
    Path { path: String },
    Selector { selector: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Field {
    Structured {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#type: Option<FieldType>,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolve_against: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<serde_json::Value>,
    },
    Dom {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#type: Option<FieldType>,
        selector: String,
        extract: String, // "text" | "html" | "attr:<name>"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolve_url: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType { String, Url, Datetime, Number, Bool }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Trim { field: String },
    Lower { field: String },
    Upper { field: String },
    RegexReplace { field: String, pattern: String, replacement: String },
    ParseDate { field: String },
    Coerce { field: String, to: CoerceTarget },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoerceTarget { Number, Bool }

/// JSON Schema for the Plan, sent to Gemini as the structured-output schema.
/// Kept as a const string so we don't drag in `schemars` for derivation.
pub const PLAN_JSON_SCHEMA: &str = include_str!("../schemas/plan.schema.json");

/// Validate a parsed Plan beyond what serde catches (cross-field invariants).
pub fn validate(plan: &Plan) -> Result<(), String> {
    match plan.kind {
        PlanKind::Listing => {
            if plan.items.is_none() {
                return Err("listing plan requires `items`".into());
            }
        }
        PlanKind::Event => {
            if plan.items.is_some() {
                return Err("event plan must not include `items`".into());
            }
        }
    }
    match (&plan.source, &plan.items) {
        (PlanSource::Dom, Some(ItemsLocator::Path { .. })) => {
            return Err("source=dom requires items.selector, not items.path".into());
        }
        (PlanSource::NextData | PlanSource::JsonLd, Some(ItemsLocator::Selector { .. })) => {
            return Err("source=next_data|json_ld requires items.path, not items.selector".into());
        }
        _ => {}
    }
    if plan.fields.is_empty() {
        return Err("at least one field is required".into());
    }
    Ok(())
}
