pub mod web;
pub mod ws;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use soundnet_protocol::{PeerPortsPush, Route, ServerMsg, StateSnapshot};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::iface;
use crate::state::PeerRecord;

#[derive(Debug, Deserialize)]
pub struct GossipQuery {
    /// When explicitly `false` (from another engine), don't gossip back or
    /// we'll create an add/remove loop.
    #[serde(default = "default_true")]
    gossip: bool,
}

fn default_true() -> bool { true }

use crate::routing;
use crate::state::EngineState;

pub async fn serve(state: Arc<EngineState>, addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/api/state", get(state_handler))
        .route("/api/routes", post(add_route_handler))
        .route("/api/routes/:id", delete(remove_route_handler))
        .route("/api/rescan", post(rescan_handler))
        .route("/api/interface", post(set_interface_handler))
        .route("/api/peer-ports", post(peer_ports_handler))
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
pub async fn snapshot(state: &Arc<EngineState>) -> StateSnapshot {
    let self_node = state.self_node();

    let local_ports: Vec<_> = state.local_ports.iter().map(|e| e.value().clone()).collect();

    let mut remote_ports = Vec::new();
    let mut nodes = vec![self_node.clone()];
    for entry in state.peers.iter() {
        nodes.push(entry.node.clone());
        remote_ports.extend(entry.ports.iter().cloned());
    }
    let routes: Vec<_> = state.routes.iter().map(|e| e.value().clone()).collect();
    let manual_hosts = state.manual_hosts.read().await.clone();
    let interfaces = iface::list_interfaces();
    let selected_interface = state.selected_interface.read().await.clone();

    StateSnapshot {
        self_node,
        nodes,
        local_ports,
        remote_ports,
        routes,
        manual_hosts,
        interfaces,
        selected_interface,
    }
}

async fn state_handler(State(state): State<Arc<EngineState>>) -> impl IntoResponse {
    Json(snapshot(&state).await)
}

async fn add_route_handler(
    State(state): State<Arc<EngineState>>,
    Query(q): Query<GossipQuery>,
    Json(mut route): Json<Route>,
) -> impl IntoResponse {
    if route.id.is_empty() {
        route.id = uuid::Uuid::new_v4().to_string();
    }
    match routing::apply_route(&state, route.clone(), q.gossip).await {
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
    Query(q): Query<GossipQuery>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    routing::remove_route(&state, &id, q.gossip).await;
    StatusCode::NO_CONTENT
}

async fn rescan_handler(State(state): State<Arc<EngineState>>) -> impl IntoResponse {
    match rescan_and_broadcast(&state).await {
        Ok(count) => (StatusCode::OK, format!("{count}")).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SetInterfaceBody {
    /// Interface name to pin, or `null`/omitted for automatic.
    #[serde(default)]
    name: Option<String>,
}

async fn set_interface_handler(
    State(state): State<Arc<EngineState>>,
    Json(body): Json<SetInterfaceBody>,
) -> impl IntoResponse {
    match set_interface_and_broadcast(&state, body.name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
    }
}

/// Apply a new interface selection, then push a fresh state snapshot to every
/// connected UI so every open sidebar reflects the new `self_node.addr` and
/// selection immediately — mirrors `rescan_and_broadcast`.
pub async fn set_interface_and_broadcast(
    state: &Arc<EngineState>,
    name: Option<String>,
) -> anyhow::Result<()> {
    iface::set_selected(state, name).await?;
    let snap = snapshot(state).await;
    let _ = state.events.send(soundnet_protocol::ServerMsg::State { snapshot: snap });
    Ok(())
}

/// Re-enumerate local ALSA ports, push a fresh state snapshot to every
/// connected UI, and tell every known peer about the new port list (mDNS
/// TXT records don't carry ports, so peers who discovered us earlier would
/// otherwise keep showing our stale port list forever). Returns the new
/// port count.
pub async fn rescan_and_broadcast(state: &Arc<EngineState>) -> anyhow::Result<usize> {
    crate::audio::devices::refresh(state)?;
    // Tones are always registered; refresh() only drops non-tone entries.
    crate::tone::register_default_tones(state);
    let count = state.local_ports.len();
    let snap = snapshot(state).await;
    let _ = state
        .events
        .send(soundnet_protocol::ServerMsg::State { snapshot: snap });
    crate::discovery::push_ports_to_peers(state);
    Ok(count)
}

/// Receive a peer's updated port list (see `discovery::push_ports_to_peers`)
/// and refresh our cached copy of it.
async fn peer_ports_handler(
    State(state): State<Arc<EngineState>>,
    Json(push): Json<PeerPortsPush>,
) -> impl IntoResponse {
    if push.node.id == state.identity.node_id {
        // Shouldn't happen, but never let a node overwrite its own record.
        return StatusCode::BAD_REQUEST;
    }
    state.peers.insert(
        push.node.id.clone(),
        PeerRecord { node: push.node.clone(), ports: push.ports.clone() },
    );
    let _ = state.events.send(ServerMsg::NodeAppeared {
        node: push.node,
        ports: push.ports,
    });
    StatusCode::NO_CONTENT
}
