use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::stream;
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

use crate::{
    cluster::AppState,
    models::{
        ActionResponse, PeerSpeedtestRequest, ShutdownRequest, SpeedtestRequest, UpdateRequest,
    },
    power, screenshot, speedtest,
};

const DEFAULT_SHUTDOWN_DELAY_SECONDS: u64 = 0;
const MAX_SHUTDOWN_DELAY_SECONDS: u64 = 3600;
const MAX_SHUTDOWN_REASON_CHARS: usize = 200;

pub async fn serve(state: Arc<AppState>, bind: SocketAddr) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/peers", get(peers))
        .route("/screenshot", get(screenshot_endpoint))
        .route("/speedtest/bytes", get(speedtest_bytes))
        .route("/speedtest/internet", post(speedtest_internet))
        .route("/speedtest/peer", post(speedtest_peer))
        .route("/power/wake/{host}", post(wake))
        .route("/power/shutdown", post(shutdown))
        .route("/update", post(update))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")
}

async fn health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.health().await))
}

async fn metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    let metrics = state.local_metrics().await.map_err(ApiError::internal)?;
    Ok(Json(metrics))
}

async fn peers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    Ok(Json(state.cluster_status().await))
}

async fn screenshot_endpoint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    if !state.config.node.allow_screenshot {
        return Err(ApiError::forbidden(
            "screenshots are not allowed on this node",
        ));
    }
    let _permit = state
        .screenshot_limiter
        .try_acquire()
        .map_err(|_| ApiError::conflict("screenshot already running on this node"))?;
    let capture = screenshot::capture().await.map_err(ApiError::internal)?;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(CONTENT_TYPE, HeaderValue::from_static(capture.content_type));
    response_headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"", capture.filename))
            .map_err(ApiError::internal)?,
    );

    Ok((response_headers, capture.bytes))
}

#[derive(Debug, Deserialize)]
struct SpeedtestBytesQuery {
    bytes: Option<u64>,
}

async fn speedtest_bytes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SpeedtestBytesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    let bytes = speedtest::requested_bytes(
        query.bytes,
        state.config.speedtest.peer_bytes,
        state.config.speedtest.max_bytes,
    )
    .map_err(ApiError::bad_request)?;
    let permit = state
        .speedtest_limiter
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::conflict("speedtest already running on this node"))?;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response_headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.to_string()).map_err(ApiError::internal)?,
    );

    Ok((response_headers, speedtest_stream(bytes, permit)))
}

async fn speedtest_internet(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Option<Json<SpeedtestRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    let request = body
        .map(|Json(body)| body)
        .unwrap_or(SpeedtestRequest { bytes: None });
    let _permit = state
        .speedtest_limiter
        .try_acquire()
        .map_err(|_| ApiError::conflict("speedtest already running on this node"))?;
    let result = speedtest::internet_download(
        &state.speedtest_http,
        &state.config.speedtest,
        &state.config.node.id,
        request.bytes,
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(result))
}

async fn speedtest_peer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<PeerSpeedtestRequest>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    let _permit = state
        .speedtest_limiter
        .try_acquire()
        .map_err(|_| ApiError::conflict("speedtest already running on this node"))?;
    let target = state
        .peer_config(&request.peer_id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("unknown peer {}", request.peer_id)))?;
    let result = speedtest::peer_download(
        &state.speedtest_http,
        &state.shared_secret,
        &state.config.node.id,
        &target,
        request.bytes,
        state.config.speedtest.peer_bytes,
        state.config.speedtest.max_bytes,
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(result))
}

async fn wake(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(host): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    let peer = state
        .peer_config(&host)
        .await
        .ok_or_else(|| ApiError::not_found(format!("unknown host {host}")))?;
    let mac = peer
        .wol_mac
        .as_deref()
        .ok_or_else(|| ApiError::bad_request(format!("host {} has no wol_mac", peer.id)))?;
    let broadcast = peer
        .wol_broadcast
        .as_deref()
        .ok_or_else(|| ApiError::bad_request(format!("host {} has no wol_broadcast", peer.id)))?;

    power::wake_on_lan(mac, broadcast).map_err(ApiError::internal)?;
    Ok(Json(ActionResponse {
        ok: true,
        message: format!("sent Wake-on-LAN packet for {}", peer.id),
    }))
}

async fn shutdown(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Option<Json<ShutdownRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &headers)?;
    if !state.config.node.allow_shutdown {
        return Err(ApiError::forbidden("shutdown is not allowed on this node"));
    }
    state
        .ensure_restart_allowed()
        .await
        .map_err(|err| ApiError::forbidden(err.to_string()))?;

    let request = body.map(|Json(body)| body).unwrap_or(ShutdownRequest {
        delay_seconds: Some(DEFAULT_SHUTDOWN_DELAY_SECONDS),
        reason: Some("requested by distributed-watchdog".to_string()),
    });
    let delay_seconds = request
        .delay_seconds
        .unwrap_or(DEFAULT_SHUTDOWN_DELAY_SECONDS);
    if delay_seconds > MAX_SHUTDOWN_DELAY_SECONDS {
        return Err(ApiError::bad_request(format!(
            "delay_seconds must not exceed {MAX_SHUTDOWN_DELAY_SECONDS}"
        )));
    }
    let reason = request
        .reason
        .unwrap_or_else(|| "requested by distributed-watchdog".to_string());
    if reason.chars().count() > MAX_SHUTDOWN_REASON_CHARS {
        return Err(ApiError::bad_request(format!(
            "reason must not exceed {MAX_SHUTDOWN_REASON_CHARS} characters"
        )));
    }
    let actual_delay_seconds =
        power::shutdown_local(delay_seconds, &reason).map_err(ApiError::internal)?;
    Ok(Json(ActionResponse {
        ok: true,
        message: render_shutdown_message(actual_delay_seconds),
    }))
}

async fn update(
    State(state): State<Arc<AppState>>,
    Json(request): Json<UpdateRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let result = state
        .schedule_update_request(&request)
        .await
        .map_err(ApiError::update_error)?;
    Ok(Json(result))
}

fn render_shutdown_message(delay_seconds: u64) -> String {
    if delay_seconds == 0 {
        "shutdown requested".to_string()
    } else {
        format!("shutdown scheduled in {delay_seconds}s")
    }
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(value) = headers.get("x-watchdog-secret") else {
        return Err(ApiError::unauthorized("missing x-watchdog-secret"));
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::unauthorized("invalid x-watchdog-secret"))?;
    if value
        .as_bytes()
        .ct_eq(state.shared_secret.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(ApiError::unauthorized("invalid x-watchdog-secret"));
    }
    Ok(())
}

fn speedtest_stream(total_bytes: u64, permit: tokio::sync::OwnedSemaphorePermit) -> Body {
    const CHUNK_BYTES: u64 = 64 * 1024;
    let stream = stream::unfold((total_bytes, permit), |(remaining, permit)| async move {
        if remaining == 0 {
            return None;
        }

        let chunk_size = remaining.min(CHUNK_BYTES);
        let bytes = Bytes::from(vec![0u8; chunk_size as usize]);
        Some((
            Ok::<Bytes, Infallible>(bytes),
            (remaining - chunk_size, permit),
        ))
    });

    Body::from_stream(stream)
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    fn bad_request(message: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_string(),
        }
    }

    fn internal(err: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }

    fn update_error(err: impl std::fmt::Display) -> Self {
        let raw = err.to_string();
        let message = safe_update_message(&raw);
        let status = if raw.contains("already running") || raw.contains("duplicate update") {
            StatusCode::CONFLICT
        } else if raw.contains("disabled")
            || raw.contains("not configured")
            || raw.contains("not allowed")
            || raw.contains("not the active leader")
            || raw.contains("without a viable successor")
        {
            StatusCode::FORBIDDEN
        } else if raw.contains("stale")
            || raw.contains("invalid")
            || raw.contains("mismatch")
            || raw.contains("leadership state")
            || raw.contains("current leader")
        {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self { status, message }
    }
}

fn safe_update_message(raw: &str) -> String {
    let safe_messages = [
        "updates are disabled on this node",
        "update.command is not configured",
        "update.command program is empty",
        "update launcher timed out",
        "update launcher failed",
        "update launcher could not start",
        "duplicate update operation",
        "update already running",
        "leadership state is not initialized",
        "refusing to restart active leader without a viable successor",
        "this node is not the active leader",
        "update cluster mismatch",
        "update target mismatch",
        "stale update request",
        "update request was not issued by the current leader",
        "invalid update signature",
        "invalid update operation id",
    ];
    if safe_messages.contains(&raw) {
        raw.to_string()
    } else {
        "update request failed; check local update.log".to_string()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(ActionResponse {
                ok: false,
                message: self.message,
            }),
        )
            .into_response()
    }
}
