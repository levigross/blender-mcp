use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use blender_mcp_protocol::BridgeOperation;
use blender_mcp_transport::BlenderBridge;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{scheme::SchemeHandle, server::BlenderMcp};

pub const DEFAULT_MAX_MCP_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    allowed_hosts: Arc<HashSet<String>>,
    allowed_origins: Arc<HashSet<String>>,
    bearer_token: Option<Arc<[u8]>>,
}

impl SecurityPolicy {
    pub fn new(
        allowed_hosts: impl IntoIterator<Item = String>,
        allowed_origins: impl IntoIterator<Item = String>,
        bearer_token: Option<String>,
    ) -> Result<Self, String> {
        let allowed_hosts = allowed_hosts.into_iter().collect::<HashSet<_>>();
        if allowed_hosts.is_empty() {
            return Err("at least one allowed Host authority is required".to_owned());
        }
        let allowed_origins = allowed_origins.into_iter().collect::<HashSet<_>>();
        let bearer_token = bearer_token
            .map(|token| {
                if token.is_empty() {
                    Err("bearer token must not be empty".to_owned())
                } else {
                    Ok(Arc::<[u8]>::from(token.into_bytes()))
                }
            })
            .transpose()?;
        Ok(Self {
            allowed_hosts: Arc::new(allowed_hosts),
            allowed_origins: Arc::new(allowed_origins),
            bearer_token,
        })
    }

    pub fn allowed_hosts(&self) -> Vec<String> {
        self.allowed_hosts.iter().cloned().collect()
    }

    pub fn allowed_origins(&self) -> Vec<String> {
        self.allowed_origins.iter().cloned().collect()
    }
}

#[derive(Clone)]
struct RuntimeState {
    bridge: Arc<dyn BlenderBridge>,
    worker: SchemeHandle,
    started: Instant,
}

impl std::fmt::Debug for RuntimeState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeState")
            .field("worker", &self.worker)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

pub fn router(
    handler: BlenderMcp,
    bridge: Arc<dyn BlenderBridge>,
    worker: SchemeHandle,
    security: SecurityPolicy,
    shutdown: &CancellationToken,
    maximum_body_bytes: usize,
) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(false)
        .with_allowed_hosts(security.allowed_hosts())
        .with_allowed_origins(security.allowed_origins())
        .with_max_request_body_bytes(maximum_body_bytes)
        .with_cancellation_token(shutdown.child_token());
    // Stateless requests call this factory per request, so every clone must share the
    // session registry and tasks. Building workers here would discard persistent Scheme state.
    let mcp = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let runtime = RuntimeState {
        bridge,
        worker,
        started: Instant::now(),
    };
    Router::new()
        .route("/healthz", get(healthz))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(security, enforce_security))
        .with_state(runtime)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    server: &'static str,
    uptime_secs: u64,
    backend: &'static str,
    bridge_address: String,
    bridge_connected: bool,
    blender_process_running: Option<bool>,
    blender: Option<Value>,
    operator_count: usize,
    catalog_revision: String,
    steel_state: &'static str,
    steel_queue: usize,
    recent_blender_logs: Vec<String>,
}

async fn healthz(State(runtime): State<RuntimeState>) -> impl IntoResponse {
    let bridge_health = runtime.bridge.health().await;
    let blender = runtime
        .bridge
        .request(BridgeOperation::ControlStatus, Duration::from_secs(2))
        .await
        .ok()
        .and_then(|response| response.result);
    let catalog = runtime.worker.catalog();
    let steel = runtime.worker.status();
    let backend = match bridge_health.mode {
        blender_mcp_transport::BridgeMode::Live => "live",
        blender_mcp_transport::BridgeMode::Headless => "headless",
    };
    let status = if bridge_health.connected {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        axum::Json(HealthResponse {
            server: "ready",
            uptime_secs: runtime.started.elapsed().as_secs(),
            backend,
            bridge_address: bridge_health.address.to_string(),
            bridge_connected: bridge_health.connected,
            blender_process_running: bridge_health.process_running,
            blender,
            operator_count: catalog.operators.len(),
            catalog_revision: catalog.revision,
            steel_state: steel.state,
            steel_queue: steel.queued,
            recent_blender_logs: bridge_health.recent_logs,
        }),
    )
}

async fn enforce_security(
    State(policy): State<SecurityPolicy>,
    request: Request,
    next: Next,
) -> Response {
    if !allowed_header(request.headers(), header::HOST, &policy.allowed_hosts, true) {
        return (StatusCode::FORBIDDEN, "Host is not allowed").into_response();
    }
    if !allowed_header(
        request.headers(),
        header::ORIGIN,
        &policy.allowed_origins,
        false,
    ) {
        return (StatusCode::FORBIDDEN, "Origin is not allowed").into_response();
    }
    if let Some(expected) = policy.bearer_token.as_ref() {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::as_bytes);
        if !supplied.is_some_and(|supplied| constant_time_eq(supplied, expected)) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "A valid bearer token is required",
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn allowed_header(
    headers: &HeaderMap,
    name: header::HeaderName,
    allowed: &HashSet<String>,
    required: bool,
) -> bool {
    match headers.get(name) {
        Some(value) => value.to_str().is_ok_and(|value| allowed.contains(value)),
        None => !required,
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

/// Host authorities and browser origins a loopback deployment accepts on `port`.
pub fn loopback_authorities(port: u16) -> (Vec<String>, Vec<String>) {
    (
        vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ],
        vec![
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
            format!("http://[::1]:{port}"),
        ],
    )
}

pub fn loopback_policy(bind: SocketAddr, bearer_token: Option<String>) -> SecurityPolicy {
    let (hosts, origins) = loopback_authorities(bind.port());
    SecurityPolicy::new(hosts, origins, bearer_token)
        .expect("invariant: loopback policy contains hosts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_handles_equal_unequal_and_different_lengths() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secres"));
        assert!(!constant_time_eq(b"secret", b"secret-long"));
    }

    #[test]
    fn rejects_empty_security_configuration() {
        assert!(SecurityPolicy::new(Vec::new(), Vec::new(), None).is_err());
        assert!(
            SecurityPolicy::new(["localhost".to_owned()], Vec::new(), Some(String::new())).is_err()
        );
    }
}
