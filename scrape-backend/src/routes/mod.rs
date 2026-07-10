use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub mod data;
pub mod plans;
pub mod sessions;

pub fn build(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Serve the React Router SPA build (dashboard/build/client). API routes
    // match first; every other path falls through to ServeDir, which serves
    // static assets and, for unknown paths (client-side routes like
    // /sessions/:id), the SPA's index.html so deep links / refreshes work.
    // Use `.fallback` (not `.not_found_service`, which forces a 404 status) so
    // deep links to client routes return index.html with a 200.
    let dist = state.config().dashboard.dist_path.clone();
    let index = format!("{dist}/index.html");
    let spa = ServeDir::new(dist).fallback(ServeFile::new(index));

    Router::new()
        .merge(sessions::router())
        .merge(plans::router())
        .merge(data::router())
        .fallback_service(spa)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
