//! Podimo-to-RSS proxy library: [`app`] builds the Axum router, [`AppState`]
//! is the shared state, and the `cache`/`config`/`podimo`/`telemetry` modules
//! are exposed for the binary and integration tests.

pub(crate) mod blocklist;
pub mod cache;
pub mod config;
pub(crate) mod episode_files;
pub(crate) mod error;
pub(crate) mod handlers;
pub mod library;
pub(crate) mod middleware;
pub mod podimo;
pub(crate) mod state;
pub mod telemetry;
pub(crate) mod templates;
pub(crate) mod util;

use axum::Router;

pub use state::AppState;

/// Build the Axum app wired with shared state and middleware.
pub async fn app(state: AppState) -> anyhow::Result<Router> {
    let router = Router::new()
        .merge(handlers::healthz::router())
        .merge(handlers::index::router())
        .merge(handlers::feed::router())
        .merge(handlers::audiobook::router())
        .merge(handlers::library::router())
        .merge(handlers::setup::router())
        .merge(handlers::stream::router())
        .fallback(handlers::not_found::fallback)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::after_request,
        ))
        .with_state(state);

    Ok(router)
}
