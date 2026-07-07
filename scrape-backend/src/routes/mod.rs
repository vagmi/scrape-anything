use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub mod dashboard;
pub mod data;
pub mod plans;
pub mod sessions;

pub fn build(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Static dashboard shell (Vite build). API + /ui fragment routes match
    // first; anything else falls through to the static files (index.html, CSS,
    // JS). Missing files 404 — fine if the dashboard hasn't been built.
    let dist = state.config().dashboard.dist_path.clone();

    Router::new()
        .merge(sessions::router())
        .merge(plans::router())
        .merge(data::router())
        .merge(dashboard::router())
        .fallback_service(ServeDir::new(dist))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
