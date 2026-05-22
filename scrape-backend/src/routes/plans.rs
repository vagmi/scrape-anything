use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/plans/lookup", get(lookup))
}

#[derive(Deserialize)]
pub struct LookupQ {
    pub host: String,
    pub route: String,
    pub intention_hash: String,
}

async fn lookup(State(s): State<AppState>, Query(q): Query<LookupQ>) -> AppResult<Json<Value>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, plan_json, rationale FROM plans
         WHERE host = ?1 AND route_pattern = ?2 AND intention_hash = ?3 AND invalidated = 0
         ORDER BY last_used_at DESC LIMIT 1",
    )
    .bind(&q.host)
    .bind(&q.route)
    .bind(&q.intention_hash)
    .fetch_optional(s.db())
    .await?;

    match row {
        Some((id, plan_json, rationale)) => {
            let plan: Value = serde_json::from_str(&plan_json).unwrap_or(Value::Null);
            Ok(Json(json!({
                "hit": true,
                "plan_id": id,
                "plan": plan,
                "rationale": rationale,
            })))
        }
        None => Ok(Json(json!({ "hit": false }))),
    }
}
