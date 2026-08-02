//! Config API + hosting for the settings web app.
//!
//! Binds to localhost only. All /api routes require a bearer token
//! (config/api-token, generated on first run) so a malicious web page
//! cannot reconfigure the censor via drive-by JS; CORS is open so the
//! externally-hosted copy of the web app can push packages here.
//!
//! Routes:
//!   GET  /api/package   -> the stored package document
//!   PUT  /api/package   -> validate, persist, resolve, and apply live
//!   GET  /api/status    -> app + effective (resolved) settings
//!   /                   -> the built web app (webapp/dist), if present

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::overlay::OverlayHandle;
use crate::settings::{Effective, Package};

pub struct ServerState {
    pub package: Mutex<Package>,
    pub effective: Arc<RwLock<Effective>>,
    pub overlay: OverlayHandle,
    pub package_path: PathBuf,
    pub token: String,
}

pub fn load_or_create_token(path: &PathBuf) -> anyhow::Result<String> {
    if let Ok(token) = std::fs::read_to_string(path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let mut bytes = [0u8; 24];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &token)?;
    Ok(token)
}

fn authed(state: &ServerState, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.token)
}

async fn get_package(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let package = state.package.lock().unwrap().clone();
    Json(package).into_response()
}

async fn put_package(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(package): Json<Package>,
) -> Response {
    if !authed(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    // Unknown layer names are a client bug worth rejecting early.
    for layer in &package.layers {
        if !package.named_configs.iter().any(|c| &c.name == layer) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("layer '{layer}' has no matching named config"),
            )
                .into_response();
        }
    }
    // Same for text-set references in any censor patch.
    let patches = package
        .named_configs
        .iter()
        .map(|c| &c.settings)
        .chain(std::iter::once(&package.overrides));
    for censor in patches.filter_map(|p| p.censor.as_ref()) {
        for set in censor.text_overlay.iter().flat_map(|t| t.sets.iter()) {
            if !package.text_sets.iter().any(|s| &s.name == set) {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("text set '{set}' is not defined in the package"),
                )
                    .into_response();
            }
        }
    }
    let effective = package.resolve();
    if let Err(e) = package.save(&state.package_path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("save failed: {e}"))
            .into_response();
    }
    *state.package.lock().unwrap() = package;
    *state.effective.write().unwrap() = effective.clone();
    if let Err(e) = state.overlay.set_style(effective.censor.clone()) {
        tracing::warn!("could not push style to overlay: {e}");
    }
    tracing::info!("configuration package applied");
    Json(effective).into_response()
}

async fn get_status(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let effective = state.effective.read().unwrap().clone();
    Json(serde_json::json!({
        "app": "betamacs",
        "version": env!("CARGO_PKG_VERSION"),
        "effective": effective,
    }))
    .into_response()
}

/// Run the server on a dedicated thread with its own tokio runtime.
pub fn spawn(state: Arc<ServerState>, port: u16, webapp_dir: PathBuf) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async move {
            let api = Router::new()
                .route("/api/package", get(get_package).put(put_package))
                .route("/api/status", get(get_status))
                .layer(CorsLayer::very_permissive())
                .with_state(state);
            let app = if webapp_dir.join("index.html").exists() {
                api.fallback_service(ServeDir::new(webapp_dir))
            } else {
                tracing::warn!(
                    "webapp not built ({} missing); serving API only",
                    webapp_dir.join("index.html").display()
                );
                api
            };
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!("settings server on http://{addr}");
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::error!("settings server exited: {e}");
                    }
                }
                Err(e) => tracing::error!("settings server could not bind {addr}: {e}"),
            }
        });
    });
}
