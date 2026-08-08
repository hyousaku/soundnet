//! Per-connection WebSocket handler. Sends an initial state snapshot, then
//! forwards broadcasted events to the client and (limited) client → server
//! commands back to the engine.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use soundnet_protocol::{ClientMsg, ServerMsg};
use std::sync::Arc;

use crate::{discovery, routing};
use crate::state::EngineState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<EngineState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| serve_socket(socket, state))
}

async fn serve_socket(socket: WebSocket, state: Arc<EngineState>) {
    let (mut sink, mut stream) = socket.split();

    // Snapshot on connect so the browser has a full picture up front.
    let snap = super::snapshot(&state).await;
    if let Ok(json) = serde_json::to_string(&ServerMsg::State { snapshot: snap }) {
        let _ = sink.send(Message::Text(json)).await;
    }

    // Fan-out from the engine's broadcast to this socket.
    let mut rx = state.events.subscribe();
    let send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if sink.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Read client -> server messages.
    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        let parsed: Result<ClientMsg, _> = serde_json::from_str(&text);
        match parsed {
            Ok(ClientMsg::Hello) => {}
            Ok(ClientMsg::AddRoute { mut route }) => {
                if route.id.is_empty() {
                    route.id = uuid::Uuid::new_v4().to_string();
                }
                if let Err(err) = routing::apply_route(&state, route, true).await {
                    tracing::warn!("ws add_route failed: {err:#}");
                    let _ = state
                        .events
                        .send(ServerMsg::Error { message: format!("{err:#}") });
                }
            }
            Ok(ClientMsg::RemoveRoute { id }) => {
                routing::remove_route(&state, &id, true).await;
            }
            Ok(ClientMsg::UpdateSpec { id, spec }) => {
                if let Some(mut route) = state.routes.get(&id).map(|r| r.clone()) {
                    route.spec = spec;
                    if let Err(err) = routing::apply_route(&state, route.clone(), true).await {
                        tracing::warn!("ws update_spec failed: {err:#}");
                    } else {
                        let _ = state.events.send(ServerMsg::RouteUpdated { route });
                    }
                }
            }
            Ok(ClientMsg::AddManualHost { addr, port }) => {
                discovery::add_manual(&state, addr, port).await;
            }
            Ok(ClientMsg::RemoveManualHost { addr, port }) => {
                discovery::remove_manual(&state, &addr, port).await;
            }
            Err(err) => {
                tracing::warn!("ws bad message: {err}");
            }
        }
    }

    send_task.abort();
}
