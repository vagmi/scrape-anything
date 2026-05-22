use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub mod sessions;
pub mod plans;

pub fn build(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .merge(sessions::router())
        .merge(plans::router())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
