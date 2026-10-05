//! Raw HTTP checks for the concerns an MCP client abstracts away: the outer security
//! middleware, rejected methods, body limits, wire framing, and the health endpoint.
//!
//! Protocol-level behaviour is covered in `mcp_protocol.rs` using the real `rmcp` client.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode, header},
};
use blender_mcp_protocol::{BridgeOperation, BridgeResponse, OperatorCatalog, PROTOCOL_VERSION};
use blender_mcp_server::{
    http::{self, SecurityPolicy},
    scheme::{SchemeSettings, SchemeWorker},
    server::BlenderMcp,
};
use blender_mcp_transport::{BlenderBridge, BridgeHealth, BridgeMode, TransportError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

const HOST: &str = "127.0.0.1:8000";
const PROTOCOL: &str = "2026-07-28";
const CATALOG_REVISION: &str = "test-revision";
const BLENDER_VERSION: &str = "5.2.1";

#[derive(Debug)]
struct MockBridge;

#[async_trait]
impl BlenderBridge for MockBridge {
    async fn request(
        &self,
        operation: BridgeOperation,
        _request_timeout: Duration,
    ) -> Result<BridgeResponse, TransportError> {
        let value = match operation {
            BridgeOperation::ControlStatus => json!({
                "blender_version": BLENDER_VERSION,
                "operator_count": 0,
                "background": true,
                "state": "busy",
            }),
            _ => json!(null),
        };
        Ok(BridgeResponse::success(1, value))
    }

    async fn health(&self) -> BridgeHealth {
        BridgeHealth {
            mode: BridgeMode::Headless,
            address: HOST.parse().expect("mock bridge address"),
            connected: true,
            process_running: Some(true),
            recent_logs: vec!["stdout: Blender quit".to_owned()],
        }
    }
}

struct Harness {
    router: Router,
    worker: SchemeWorker,
}

impl Harness {
    async fn start(bearer_token: Option<String>) -> Self {
        Self::with_policy(http::loopback_policy(
            HOST.parse().expect("bind address"),
            bearer_token,
        ))
        .await
    }

    async fn with_policy(security: SecurityPolicy) -> Self {
        let catalog = OperatorCatalog {
            protocol_version: PROTOCOL_VERSION,
            revision: CATALOG_REVISION.to_owned(),
            blender_version: BLENDER_VERSION.to_owned(),
            operators: Vec::new(),
        };
        let worker = SchemeWorker::spawn(
            Arc::new(MockBridge),
            catalog,
            tokio::runtime::Handle::current(),
            SchemeSettings {
                default_timeout: Duration::from_secs(5),
                maximum_timeout: Duration::from_secs(30),
                library: None,
            },
        )
        .await
        .expect("Steel worker starts");
        let router = http::router(
            BlenderMcp::new(worker.handle()),
            Arc::new(MockBridge),
            worker.handle(),
            security,
            &CancellationToken::new(),
            http::DEFAULT_MAX_MCP_BODY_BYTES,
        );
        Self { router, worker }
    }

    async fn send(&self, request: Request<Body>) -> Response<Body> {
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds")
    }

    async fn shutdown(self) {
        drop(self.router);
        self.worker.shutdown().await;
    }
}

/// Build a spec-shaped stateless request; `mcp_protocol.rs` proves these headers are
/// what the real client sends.
fn mcp_request(method: &str, mut params: Value, bearer_token: Option<&str>) -> Request<Body> {
    if let Some(object) = params.as_object_mut() {
        object.insert(
            "_meta".to_owned(),
            json!({
                "io.modelcontextprotocol/protocolVersion": PROTOCOL,
                "io.modelcontextprotocol/clientCapabilities": {},
            }),
        );
    }
    let payload = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
        .header("mcp-method", method);
    if let Some(name) = params.get("name").and_then(Value::as_str) {
        builder = builder.header("mcp-name", name);
    }
    if let Some(token) = bearer_token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(payload.to_string()))
        .expect("request builds")
}

fn header_value(response: &Response<Body>, name: header::HeaderName) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body collects");
    String::from_utf8(bytes.to_vec()).expect("body is UTF-8")
}

#[tokio::test]
async fn a_tool_call_is_framed_as_request_scoped_sse() {
    let harness = Harness::start(None).await;
    let response = harness
        .send(mcp_request(
            "tools/call",
            json!({"name": "scheme_eval", "arguments": {"code": "(+ 1 2)"}}),
            None,
        ))
        .await;

    let status = response.status();
    let content_type = header_value(&response, header::CONTENT_TYPE);
    let body = body_text(response).await;
    assert_eq!(status, StatusCode::OK, "tool call failed: {body}");
    assert!(
        content_type.starts_with("text/event-stream"),
        "json_response(false) must preserve SSE, got {content_type}"
    );

    let frame = body
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .unwrap_or_else(|| panic!("no SSE data frame in: {body}"));
    let message: Value = serde_json::from_str(frame.trim()).expect("SSE data frame is JSON");
    assert_eq!(message["result"]["content"][0]["text"], "3");
    harness.shutdown().await;
}

#[tokio::test]
async fn get_and_delete_on_mcp_are_rejected() {
    let harness = Harness::start(None).await;
    for method in ["GET", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/mcp")
            .header(header::HOST, HOST)
            .header(header::ACCEPT, "text/event-stream")
            .body(Body::empty())
            .expect("request builds");
        assert_eq!(
            harness.send(request).await.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} /mcp should be rejected without legacy two-endpoint SSE"
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn host_is_required_and_must_match_the_bound_authority() {
    let harness = Harness::start(None).await;

    for host in [None, Some("evil.example.com"), Some("127.0.0.1:9999")] {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        let request = builder.body(Body::from("{}")).expect("request builds");
        assert_eq!(
            harness.send(request).await.status(),
            StatusCode::FORBIDDEN,
            "host {host:?} should be refused"
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn origin_is_normalized_to_forbidden_before_rmcp_sees_it() {
    let harness = Harness::start(None).await;

    // Includes a malformed value, which rmcp alone would answer with 400.
    for origin in ["https://evil.example.com", "http://127.0.0.1:9999", "null"] {
        let mut request = mcp_request("tools/list", json!({}), None);
        request.headers_mut().insert(
            header::ORIGIN,
            header::HeaderValue::from_str(origin).expect("header value"),
        );
        assert_eq!(
            harness.send(request).await.status(),
            StatusCode::FORBIDDEN,
            "origin {origin} should be refused"
        );
    }

    let mut allowed = mcp_request("tools/list", json!({}), None);
    allowed.headers_mut().insert(
        header::ORIGIN,
        header::HeaderValue::from_static("http://localhost:8000"),
    );
    assert_eq!(harness.send(allowed).await.status(), StatusCode::OK);
    harness.shutdown().await;
}

#[tokio::test]
async fn bearer_credentials_are_required_when_configured() {
    let harness = Harness::start(Some("correct-horse".to_owned())).await;

    let missing = harness
        .send(mcp_request("tools/list", json!({}), None))
        .await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(header_value(&missing, header::WWW_AUTHENTICATE), "Bearer");

    let wrong = harness
        .send(mcp_request("tools/list", json!({}), Some("battery-staple")))
        .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // A prefix of the real token must not be accepted.
    let truncated = harness
        .send(mcp_request("tools/list", json!({}), Some("correct-hors")))
        .await;
    assert_eq!(truncated.status(), StatusCode::UNAUTHORIZED);

    let mut malformed = mcp_request("tools/list", json!({}), None);
    malformed.headers_mut().insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_static("Basic correct-horse"),
    );
    assert_eq!(
        harness.send(malformed).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let correct = harness
        .send(mcp_request("tools/list", json!({}), Some("correct-horse")))
        .await;
    assert_eq!(correct.status(), StatusCode::OK);

    // The health endpoint sits behind the same middleware.
    let health = Request::builder()
        .method("GET")
        .uri("/healthz")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .expect("request builds");
    assert_eq!(
        harness.send(health).await.status(),
        StatusCode::UNAUTHORIZED
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn oversized_request_bodies_are_rejected() {
    let harness = Harness::with_policy(
        SecurityPolicy::new([HOST.to_owned()], Vec::new(), None).expect("policy"),
    )
    .await;
    let oversized = "x".repeat(http::DEFAULT_MAX_MCP_BODY_BYTES + 1);
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
        .header("mcp-method", "tools/list")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"pad": oversized}})
                .to_string(),
        ))
        .expect("request builds");
    let status = harness.send(request).await.status();
    assert!(
        status == StatusCode::PAYLOAD_TOO_LARGE || status == StatusCode::BAD_REQUEST,
        "oversized bodies must be refused, got {status}"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn health_reports_each_subsystem_separately() {
    let harness = Harness::start(None).await;
    let request = Request::builder()
        .method("GET")
        .uri("/healthz")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .expect("request builds");
    let response = harness.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let health: Value = serde_json::from_str(&body_text(response).await).expect("health JSON");
    assert_eq!(health["server"], "ready");
    assert_eq!(health["backend"], "headless");
    assert_eq!(health["bridge_connected"], json!(true));
    assert_eq!(health["blender_process_running"], json!(true));
    assert_eq!(health["blender"]["blender_version"], BLENDER_VERSION);
    assert_eq!(
        health["blender"]["state"], "busy",
        "busy Blender remains observable through the control path"
    );
    assert_eq!(health["catalog_revision"], CATALOG_REVISION);
    assert_eq!(health["steel_state"], "ready");
    assert_eq!(health["steel_queue"], json!(0));
    assert_eq!(
        health["recent_blender_logs"][0], "stdout: Blender quit",
        "bounded child output should reach the operator"
    );
    harness.shutdown().await;
}
