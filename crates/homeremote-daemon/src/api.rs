//! The REST and WebSocket surface the React remote talks to.
//!
//! No authentication: this is a LAN appliance for one household, the same trust
//! model the receiver and the TV already use. It binds to the local network,
//! not the internet.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use homeremote::config::Activity;
use homeremote::device::{Command, DeviceId};
use serde::{Deserialize, Serialize};

use crate::activity;
use crate::assets;
use crate::state::{AppState, DeviceSnapshot, ServerEvent};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/devices", get(devices))
        .route("/api/devices/{id}/command", post(command))
        .route("/api/devices/{id}/pair/start", post(pair_start))
        .route("/api/devices/{id}/pair/finish", post(pair_finish))
        .route("/api/activities", get(activities))
        .route("/api/activities/{id}/start", post(start_activity))
        .route("/api/discover", post(discover))
        .route("/api/events", get(events))
        .fallback(assets::serve)
        .with_state(state)
}

/// An error that reaches the UI as JSON rather than a bare status code.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DevicesResponse {
    devices: Vec<DeviceSnapshot>,
}

async fn devices(State(state): State<Arc<AppState>>) -> Json<DevicesResponse> {
    Json(DevicesResponse {
        devices: state.snapshots().await,
    })
}

#[derive(Deserialize)]
struct CommandRequest {
    command: Command,
}

async fn command(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<CommandRequest>,
) -> Result<StatusCode, ApiError> {
    let id = DeviceId::new(id);
    let device = state.device(&id).await.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("no controllable device with id {id}"),
        )
    })?;

    device.send(request.command).await.map_err(|e| {
        // The device being off or busy is an upstream failure, not a bad
        // request — the UI shows these differently.
        ApiError(StatusCode::BAD_GATEWAY, e.to_string())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Begin pairing; the device puts a PIN on screen.
async fn pair_start(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = DeviceId::new(id);
    let entry = state
        .config
        .read()
        .await
        .device(&id)
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no device with id {id}")))?;

    let addr = entry.addr.ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "no address known for that device — run a scan first".into(),
        )
    })?;

    let pairing = homeremote::appletv::session::begin_pairing(addr, entry.port)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    *state.pending_pairing.lock().await = Some(crate::state::PendingPairing {
        device: id,
        pairing,
    });
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PinRequest {
    pin: String,
}

/// Finish pairing with the PIN from the screen and store the credentials.
async fn pair_finish(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<PinRequest>,
) -> Result<StatusCode, ApiError> {
    let id = DeviceId::new(id);
    let pending = state.pending_pairing.lock().await.take().ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            "no pairing is in progress — start one first".into(),
        )
    })?;

    if pending.device != id {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "a pairing is in progress for a different device".into(),
        ));
    }

    let credentials = pending
        .pairing
        .finish(&request.pin)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    let (device_public, client_private, device_id, client_id) = credentials.to_hex();
    {
        let mut config = state.config.write().await;
        let entry = config.device_mut(&id).ok_or_else(|| {
            ApiError(StatusCode::NOT_FOUND, "device vanished from the config".into())
        })?;
        entry.credentials = Some(homeremote::config::Credentials::AppleTv {
            device_public,
            client_private,
            device_id,
            client_id,
        });
    }
    state.save_config().await.map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("paired, but could not save the credentials: {e}"),
        )
    })?;

    // Bring the device up now that it can be controlled.
    state.reopen(&id).await;
    state.publish(ServerEvent::DevicesChanged);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivitiesResponse {
    activities: Vec<Activity>,
    current: Option<String>,
}

async fn activities(State(state): State<Arc<AppState>>) -> Json<ActivitiesResponse> {
    Json(ActivitiesResponse {
        activities: state.config.read().await.activities.clone(),
        current: state.current_activity.read().await.clone(),
    })
}

async fn start_activity(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    activity::run(&state, &id).await.map_err(|e| match e {
        activity::ActivityError::Unknown(_) => ApiError(StatusCode::NOT_FOUND, e.to_string()),
        _ => ApiError(StatusCode::BAD_GATEWAY, e.to_string()),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn discover(State(state): State<Arc<AppState>>) -> Result<Json<DevicesResponse>, ApiError> {
    let found = homeremote::discover(homeremote::discovery::DEFAULT_TIMEOUT).await;
    state.config.write().await.merge_discovered(&found);
    state.save_config().await.map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not save the config: {e}"),
        )
    })?;
    state.open_devices().await;
    state.publish(ServerEvent::DevicesChanged);

    Ok(Json(DevicesResponse {
        devices: state.snapshots().await,
    }))
}

async fn events(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| stream_events(socket, state))
}

/// Push every state change to one connected client.
///
/// This is what keeps two phones and a laptop showing the same volume.
async fn stream_events(mut socket: WebSocket, state: Arc<AppState>) {
    let mut events = state.events.subscribe();

    // Send the current picture first, so a client that connects mid-film isn't
    // blank until something happens to change.
    let snapshot = serde_json::json!({
        "type": "snapshot",
        "devices": state.snapshots().await,
        "currentActivity": *state.current_activity.read().await,
    });
    if socket
        .send(Message::Text(snapshot.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    let Ok(text) = serde_json::to_string(&event) else { continue };
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "websocket client fell behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },

            // Drain incoming frames so pings are answered and a closed socket
            // is noticed promptly.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
