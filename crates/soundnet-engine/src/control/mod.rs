pub mod web;
pub mod ws;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post};
use axum::Router;
use soundnet_protocol::{Route, StateSnapshot};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::routing;
use crate::state::EngineState;

pub async fn serve(state: Arc<EngineState>, addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/api/state", get(state_handler))
        .route("/api/routes", post(add_route_handler))
        .route("/api/routes/:id", delete(remove_route_handler))
        .route("/ws", get(ws::ws_handler))
        .fallback(web::static_handler)
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

/// Build a snapshot for a fresh client.
pub fn snapshot(state: &Arc<EngineState>) -> StateSnapshot {
    let self_node = state.self_node();

    let local_ports: Vec<_> = state.local_ports.iter().map(|e| e.value().clone()).collect();

    let mut remote_ports = Vec::new();
    let mut nodes = vec![self_node.clone()];
    for entry in state.peers.iter() {
        nodes.push(entry.node.clone());
        remote_ports.extend(entry.ports.iter().cloned());
    }
    let routes: Vec<_> = state.routes.iter().map(|e| e.value().clone()).collect();

    StateSnapshot {
        self_node,
        nodes,
        local_ports,
        remote_ports,
        routes,
    }
}

async fn state_handler(State(state): State<Arc<EngineState>>) -> impl IntoResponse {
    Json(snapshot(&state))
}

async fn add_route_handler(
    State(state): State<Arc<EngineState>>,
    Json(mut route): Json<Route>,
) -> impl IntoResponse {
    if route.id.is_empty() {
        route.id = uuid::Uuid::new_v4().to_string();
    }
    match routing::apply_route(&state, route.clone()).await {
        Ok(()) => (StatusCode::CREATED, Json(route)).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            format!("{err:#}"),
        )
            .into_response(),
    }
}

async fn remove_route_handler(
    State(state): State<Arc<EngineState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    routing::remove_route(&state, &id).await;
    StatusCode::NO_CONTENT
}
