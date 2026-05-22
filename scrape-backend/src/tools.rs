//! Tool catalog (SPEC §6). These are the read-only probes the LLM can call
//! against the extension during the agentic loop. The actual dispatch happens
//! over the HTTP protocol — the LLM emits a `tool_call` turn, the extension
//! executes the probe in-page and POSTs the result back. This module just
//! defines names, argument types, and JSON schemas for validation on both
//! sides of the wire.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    QuerySelectorAll,
    ReadJsonPath,
    ListAttributes,
    ProbePlan,
    // FetchSubpage,   // Phase 4
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolName::QuerySelectorAll => "query_selector_all",
            ToolName::ReadJsonPath    => "read_json_path",
            ToolName::ListAttributes  => "list_attributes",
            ToolName::ProbePlan       => "probe_plan",
        }
    }
}

/// JSON schema (Draft 2020-12) for each tool's `args` and `result`.
/// Used by the backend to validate both LLM-emitted args and extension-returned
/// results before either is trusted.
pub struct ToolSchema {
    pub name: ToolName,
    pub description: &'static str,
    pub args_schema: &'static str,
    pub result_schema: &'static str,
}

pub fn catalog() -> [ToolSchema; 4] {
    [
        ToolSchema {
            name: ToolName::QuerySelectorAll,
            description: "Match a CSS selector and return structural data plus a few outerHTML samples.",
            args_schema: include_str!("../schemas/tools/query_selector_all.args.json"),
            result_schema: include_str!("../schemas/tools/query_selector_all.result.json"),
        },
        ToolSchema {
            name: ToolName::ReadJsonPath,
            description: "Query embedded structured data (next_data or json_ld) by dotted path.",
            args_schema: include_str!("../schemas/tools/read_json_path.args.json"),
            result_schema: include_str!("../schemas/tools/read_json_path.result.json"),
        },
        ToolSchema {
            name: ToolName::ListAttributes,
            description: "Summarize which attributes / data-* keys appear on elements matching a selector.",
            args_schema: include_str!("../schemas/tools/list_attributes.args.json"),
            result_schema: include_str!("../schemas/tools/list_attributes.result.json"),
        },
        ToolSchema {
            name: ToolName::ProbePlan,
            description: "Dry-run a candidate plan and report stats without downloading.",
            args_schema: include_str!("../schemas/tools/probe_plan.args.json"),
            result_schema: include_str!("../schemas/tools/probe_plan.result.json"),
        },
    ]
}
