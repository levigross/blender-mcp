//! Protocol-level checks driven by the real `rmcp` client over Streamable HTTP.
//!
//! Using the shipped client rather than a hand-rolled one keeps these tests honest about
//! transport requirements the server must satisfy: SEP-2243 routing headers, stateless
//! negotiation metadata, and SSE framing.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use blender_mcp_protocol::{BridgeOperation, BridgeResponse, OperatorCatalog, PROTOCOL_VERSION};
use blender_mcp_server::{
    http,
    scheme::{SchemeSettings, SchemeWorker},
    server::BlenderMcp,
};
use blender_mcp_transport::{BlenderBridge, BridgeHealth, BridgeMode, TransportError};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt as _, ServiceExt as _,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ClientCapabilities, ClientInfo, DetailedTask, GetTaskParams, Implementation,
        ProtocolVersion, ReadResourceRequestParams, ResourceContents, TaskPayload, TaskStatus,
        UpdateTaskParams,
    },
    service::{RoleClient, RunningService},
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
            BridgeOperation::Status => json!({
                "blender_version": BLENDER_VERSION,
                "operator_count": 0,
                "background": true,
            }),
            BridgeOperation::SceneSummary => json!({"scene": "Scene", "objects": 0}),
            BridgeOperation::OperatorCall { idname, kwargs, .. } => {
                if idname == "test.slow" {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if idname == "test.fail" {
                    return Err(TransportError::Blender {
                        code: "invalid_property".to_owned(),
                        message: "property unavailable".to_owned(),
                        potentially_continuing: false,
                        data: Some(Box::new(json!({"attribute": "missing"}))),
                        retryable: false,
                        request_id: 42,
                        session_id: "test-session".to_owned(),
                        instance_id: Some("test-instance".to_owned()),
                    });
                }
                json!({"operator": idname, "kwargs": kwargs})
            }
            BridgeOperation::ContextRef => {
                json!({"$rna_ref": {"generation": 1, "id": "context", "type_name": "Context"}})
            }
            BridgeOperation::RnaCall { args, kwargs, .. } => {
                json!({"args": args, "kwargs": kwargs})
            }
            BridgeOperation::RnaItems { offset, limit, .. } => {
                json!({"offset": offset, "limit": limit})
            }
            BridgeOperation::MeshFromData {
                name,
                vertices,
                faces,
                collection,
                ..
            } => json!({"name": name, "vertices": vertices, "faces": faces,
                        "collection": collection.map(|reference| reference.id)}),
            BridgeOperation::NodeTreeBuild {
                clear,
                interface,
                nodes,
                links,
                ..
            } => json!({"clear": clear, "interface": interface, "nodes": nodes, "links": links}),
            BridgeOperation::CollectionRead {
                attribute,
                offset,
                count,
                ..
            } => json!({"attribute": attribute, "offset": offset, "count": count}),
            BridgeOperation::CollectionWrite {
                attribute,
                offset,
                values,
                ..
            } => json!({"attribute": attribute, "offset": offset, "values": values}),
            BridgeOperation::IdPropertySet { key, value, .. } => {
                json!({"key": key, "value": value})
            }
            BridgeOperation::Render { .. } => json!({"artifact": {
                "id": "frame", "name": "frame.png", "mime_type": "image/png", "path": "/tmp/frame.png"
            }}),
            BridgeOperation::Artifact { .. } => json!({
                "artifact": {"id": "frame", "name": "frame.png", "mime_type": "image/png"},
                "data_base64": "aW1hZ2U="
            }),
            _ => json!(null),
        };
        Ok(BridgeResponse::success(1, value))
    }

    async fn health(&self) -> BridgeHealth {
        BridgeHealth {
            mode: BridgeMode::Headless,
            address: "127.0.0.1:9876".parse().expect("mock bridge address"),
            connected: true,
            process_running: Some(true),
            recent_logs: Vec::new(),
        }
    }
}

/// A real `blender-mcp` HTTP service on an ephemeral loopback port.
struct TestServer {
    address: SocketAddr,
    worker: SchemeWorker,
    shutdown: CancellationToken,
    serve: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
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
                default_timeout: Duration::from_secs(10),
                maximum_timeout: Duration::from_secs(30),
            },
        )
        .await
        .expect("Steel worker starts");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral port binds");
        let address = listener.local_addr().expect("bound address");
        let shutdown = CancellationToken::new();
        let router = http::router(
            BlenderMcp::new(worker.handle()),
            Arc::new(MockBridge),
            worker.handle(),
            // The policy is derived from the port actually bound, so a real client's
            // `Host` header is accepted exactly as it would be in production.
            http::loopback_policy(address, None),
            &shutdown,
            http::DEFAULT_MAX_MCP_BODY_BYTES,
        );

        let serve_shutdown = shutdown.clone();
        let serve = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
                .await
                .expect("service runs");
        });
        Self {
            address,
            worker,
            shutdown,
            serve,
        }
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let transport =
            StreamableHttpClientTransport::from_uri(format!("http://{}/mcp", self.address));
        ().serve(transport)
            .await
            .expect("client completes the MCP handshake")
    }

    async fn shutdown(self) {
        self.shutdown.cancel();
        drop(self.serve.await);
        self.worker.shutdown().await;
    }

    async fn connect_tasks(&self) -> RunningService<RoleClient, ClientInfo> {
        ClientInfo::new(
            ClientCapabilities::builder().enable_tasks().build(),
            Implementation::new("task-test", "1"),
        )
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_uri(format!("http://{}/mcp", self.address)),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("task-capable client connects")
    }
}

/// Evaluate Scheme through a fresh client, mirroring an independent MCP caller.
async fn evaluate(server: &TestServer, code: &str) -> CallToolResult {
    let client = server.connect().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("scheme_eval").with_arguments(
                json!({"code": code})
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("scheme_eval responds");
    client.cancel().await.expect("client closes");
    result
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("scheme_eval always returns structured content")
}

#[tokio::test]
async fn tools_list_exposes_exactly_scheme_eval() {
    let server = TestServer::start().await;
    let client = server.connect().await;

    let tools = client.list_tools(None).await.expect("tools/list").tools;
    assert_eq!(tools.len(), 1, "expected exactly one tool: {tools:?}");
    assert_eq!(tools[0].name, "scheme_eval");

    let properties = tools[0]
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("scheme_eval declares an object schema");
    for property in [
        "code",
        "session",
        "timeout_secs",
        "reset",
        "include_events",
        "background",
    ] {
        assert!(
            properties.contains_key(property),
            "scheme_eval schema is missing {property}"
        );
    }

    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

#[tokio::test]
async fn server_info_advertises_tools_and_resources() {
    let server = TestServer::start().await;
    let client = server.connect().await;

    let info = client.peer_info().expect("server info after initialize");
    let implementation = info
        .server_info
        .as_ref()
        .expect("server identifies itself during initialize");
    assert_eq!(implementation.name, "blender-mcp");
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.resources.is_some());
    assert!(
        info.instructions
            .as_ref()
            .is_some_and(|text| text.contains("scheme_eval")),
        "instructions should point callers at the single tool"
    );

    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

#[tokio::test]
async fn successful_tool_call_returns_display_and_structured_content() {
    let server = TestServer::start().await;
    let result = evaluate(&server, "(+ 1 2)").await;

    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        result.content[0]
            .as_text()
            .expect("a text block")
            .text
            .trim(),
        "3"
    );
    assert_eq!(structured(&result)["result"], json!(3));
    assert_eq!(structured(&result)["catalog_revision"], CATALOG_REVISION);
    server.shutdown().await;
}

#[tokio::test]
async fn scheme_state_persists_across_distinct_clients_and_reset_is_visible() {
    let server = TestServer::start().await;

    evaluate(&server, "(define counter 41)").await;
    // A second, independent client must observe the first client's definition.
    let observed = evaluate(&server, "(begin (set! counter (+ counter 1)) counter)").await;
    assert_eq!(structured(&observed)["result"], json!(42));

    let reset_client = server.connect().await;
    let reset = reset_client
        .call_tool(
            CallToolRequestParams::new("scheme_eval").with_arguments(
                json!({"code": "(+ 0 0)", "reset": true})
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("reset evaluates");
    assert_ne!(reset.is_error, Some(true));
    reset_client.cancel().await.expect("client closes");

    // A third client sees the rebuilt environment, so the definition is gone.
    let after_reset = evaluate(&server, "counter").await;
    assert_eq!(after_reset.is_error, Some(true));
    assert_eq!(
        structured(&after_reset)["code"],
        "scheme_error",
        "an unbound identifier is a domain error, not a protocol error"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn blender_bindings_are_reachable_from_scheme() {
    let server = TestServer::start().await;
    let result = evaluate(&server, "(blender-status)").await;
    assert_eq!(
        structured(&result)["result"]["blender_version"],
        BLENDER_VERSION
    );
    server.shutdown().await;
}

#[tokio::test]
async fn optional_arguments_have_consistent_defaults_and_reject_extras() {
    let server = TestServer::start().await;
    for (source, expected) in [
        (
            r#"(op-call "object.shade_smooth")"#,
            json!({"operator": "object.shade_smooth", "kwargs": {}}),
        ),
        (
            r#"(rna-call (context-ref) "test")"#,
            json!({"args": [], "kwargs": {}}),
        ),
        (
            r#"(rna-call (context-ref) "test" (list 1))"#,
            json!({"args": [1], "kwargs": {}}),
        ),
        (
            "(rna-items (context-ref))",
            json!({"offset": 0, "limit": 100}),
        ),
        (
            "(rna-items (context-ref) 5)",
            json!({"offset": 5, "limit": 100}),
        ),
    ] {
        let result = evaluate(&server, source).await;
        assert_ne!(result.is_error, Some(true), "{source}: {result:?}");
        assert_eq!(structured(&result)["result"], expected);
    }
    for source in [
        "(op-call)",
        r#"(op-call "object.shade_smooth" (hash) (hash))"#,
        r#"(rna-call (context-ref) "test" '() (hash) 1)"#,
        "(rna-items (context-ref) 0 100 #false 1)",
    ] {
        let result = evaluate(&server, source).await;
        assert_eq!(result.is_error, Some(true), "{source}");
        assert!(
            structured(&result)["message"]
                .as_str()
                .unwrap()
                .contains("expects")
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn artifacts_travel_as_content_without_base64_in_structured_results() {
    let server = TestServer::start().await;
    for source in [
        "(render!)",
        r#"(render-to! "/tmp/frame.png")"#,
        r#"(artifact-get "frame")"#,
    ] {
        let result = evaluate(&server, source).await;
        assert_ne!(result.is_error, Some(true), "{source}: {result:?}");
        assert_eq!(result.content.len(), 2);
        assert!(result.content[1].as_image().is_some());
        let metadata = structured(&result);
        assert_eq!(metadata["artifacts"][0]["id"], "frame");
        assert!(!metadata.to_string().contains("aW1hZ2U="));
        assert!(!metadata.to_string().contains("data_base64"));
    }
    let file = evaluate(&server, r#"(render-file! "/tmp/frame.png")"#).await;
    assert_eq!(structured(&file)["result"], "/tmp/frame.png");
    assert_eq!(file.content.len(), 1);
    assert_eq!(structured(&file)["artifacts"], json!([]));
    server.shutdown().await;
}

#[tokio::test]
async fn sandbox_escapes_are_refused_as_domain_errors() {
    let server = TestServer::start().await;
    for source in [
        "(require-builtin steel/process)",
        "(require \"/etc/passwd\")",
        "(load \"secrets.scm\")",
        "(eval-string \"(+ 1 2)\")",
    ] {
        let result = evaluate(&server, source).await;
        assert_eq!(result.is_error, Some(true), "accepted: {source}");
        assert_eq!(
            structured(&result)["code"],
            "sandbox_violation",
            "rejected too late for: {source}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn evaluation_errors_are_tool_errors_rather_than_transport_failures() {
    let server = TestServer::start().await;
    let result = evaluate(&server, "(car '())").await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["code"], "scheme_error");
    assert!(
        structured(&result)["potentially_continuing"] == json!(false),
        "a pure Scheme failure never left Blender work running"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bridge_errors_preserve_receipts_without_mislabeling_later_scheme_errors() {
    let server = TestServer::start().await;
    let result = evaluate(&server, r#"(op-call "test.fail")"#).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["code"], "invalid_property");
    assert_eq!(structured(&result)["data"]["request_id"], 42);
    assert_eq!(structured(&result)["data"]["session_id"], "test-session");
    assert_eq!(
        structured(&result)["data"]["details"]["attribute"],
        "missing"
    );
    let later = evaluate(
        &server,
        r#"
        (define caught (try (lambda () (op-call "test.fail")) "caught"))
        (car '())
    "#,
    )
    .await;
    assert_eq!(structured(&later)["code"], "scheme_error");
    assert_eq!(
        structured(&evaluate(&server, "caught").await)["result"],
        "caught"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn repeated_artifacts_attach_once_and_unknown_batch_fields_are_rejected() {
    let server = TestServer::start().await;
    let repeated = evaluate(&server, r#"(artifact-get "frame") (artifact-get "frame")"#).await;
    assert_eq!(repeated.content.len(), 2);
    assert_eq!(
        structured(&repeated)["artifacts"].as_array().unwrap().len(),
        1
    );
    let typo = evaluate(
        &server,
        r#"(batch! (list
        (hash "operation" "rna_get" "reference" (context-ref) "attribute" "scene" "typo" 1)))"#,
    )
    .await;
    assert_eq!(typo.is_error, Some(true));
    assert!(
        structured(&typo)["message"]
            .as_str()
            .unwrap()
            .contains("unknown fields")
    );
    server.shutdown().await;
}

#[tokio::test]
async fn resources_list_and_read_round_trip() {
    let server = TestServer::start().await;
    let client = server.connect().await;

    let resources = client
        .list_resources(None)
        .await
        .expect("resources/list")
        .resources;
    for uri in [
        "blender-mcp://guide/getting-started",
        "blender-mcp://guide/live",
        "blender-mcp://guide/headless",
        "blender-mcp://reference/scheme",
        "blender-mcp://reference/rna",
        "blender-mcp://guide/security",
        "blender-mcp://recipes",
        "blender-mcp://runtime/catalog",
    ] {
        assert!(
            resources.iter().any(|resource| resource.uri == uri),
            "missing resource {uri}"
        );
    }

    let runtime = client
        .read_resource(ReadResourceRequestParams::new(
            "blender-mcp://runtime/catalog",
        ))
        .await
        .expect("resources/read");
    let ResourceContents::TextResourceContents { text, .. } = &runtime.contents[0] else {
        panic!("the runtime catalog must be text, not a blob");
    };
    assert!(
        text.contains(CATALOG_REVISION) && text.contains(BLENDER_VERSION),
        "the runtime catalog should report live values: {text}"
    );

    let unknown = client
        .read_resource(ReadResourceRequestParams::new("blender-mcp://nope"))
        .await;
    assert!(
        unknown.is_err(),
        "an unknown URI must surface as a protocol error"
    );

    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

#[tokio::test]
async fn build_bindings_send_typed_payloads_and_reject_malformed_geometry() {
    let server = TestServer::start().await;

    let mesh = evaluate(
        &server,
        r#"(mesh-from-data! "Tri" (list (list 0 0 0) (list 1 0 0) (list 0 1.5 0))
                            (list (list 0 1 2)) (context-ref))"#,
    )
    .await;
    assert_ne!(mesh.is_error, Some(true), "{mesh:?}");
    assert_eq!(
        structured(&mesh)["result"],
        json!({"name": "Tri", "vertices": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.5, 0.0]],
               "faces": [[0, 1, 2]], "collection": "context"})
    );

    for malformed in [
        r#"(mesh-from-data! "Bad" (list (list 0 0)) (list))"#,
        r#"(mesh-from-data! "Bad" (list (list 0 0 0)) (list (list -1 0 0)))"#,
    ] {
        let rejected = evaluate(&server, malformed).await;
        assert_eq!(rejected.is_error, Some(true), "{malformed} should fail");
    }

    let graph = evaluate(
        &server,
        r#"(node-tree! (context-ref)
             (hash "clear" #true
                   "nodes" (list (node "Scale" "ShaderNodeMath" "inputs" (list (list 1 2.5))))
                   "links" (list (link "In" "Geometry" "Scale" 0))))"#,
    )
    .await;
    assert_eq!(
        structured(&graph)["result"],
        json!({"clear": true, "interface": [],
               "nodes": [{"name": "Scale", "type": "ShaderNodeMath", "inputs": [[1, 2.5]]}],
               "links": [["In", "Geometry", "Scale", 0]]})
    );
    let malformed = evaluate(&server, r"(node-tree! (context-ref) (list 1 2))").await;
    assert_eq!(malformed.is_error, Some(true));

    let read = evaluate(&server, r#"(collection-read (context-ref) "co" 10 5)"#).await;
    assert_eq!(
        structured(&read)["result"],
        json!({"attribute": "co", "offset": 10, "count": 5})
    );
    let whole = evaluate(&server, r#"(collection-read (context-ref) "co")"#).await;
    assert_eq!(
        structured(&whole)["result"],
        json!({"attribute": "co", "offset": 0, "count": null})
    );
    let written = evaluate(
        &server,
        r#"(collection-write! (context-ref) "co" 2 (list 1.5 2 3))"#,
    )
    .await;
    assert_eq!(
        structured(&written)["result"],
        json!({"attribute": "co", "offset": 2, "values": [1.5, 2, 3]})
    );

    let tagged = evaluate(
        &server,
        r#"(prop-set! (context-ref) "role" (hash "kind" "walkable_ground"))"#,
    )
    .await;
    assert_eq!(
        structured(&tagged)["result"],
        json!({"key": "role", "value": {"kind": "walkable_ground"}})
    );

    server.shutdown().await;
}

#[tokio::test]
async fn resource_results_carry_cache_hints_required_by_2026_07_28() {
    let server = TestServer::start().await;
    let client = server.connect_tasks().await;

    let list = client.list_resources(None).await.expect("resources/list");
    assert_eq!(list.ttl_ms, Some(0));
    assert_eq!(list.cache_scope, Some(CacheScope::Public));

    let templates = client
        .list_resource_templates(None)
        .await
        .expect("resources/templates/list");
    assert_eq!(templates.ttl_ms, Some(0));
    assert_eq!(templates.cache_scope, Some(CacheScope::Public));

    let read = client
        .read_resource(ReadResourceRequestParams::new(
            "resources://blender/sessions",
        ))
        .await
        .expect("resources/read");
    assert_eq!(read.ttl_ms, Some(0));
    assert_eq!(read.cache_scope, Some(CacheScope::Private));

    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_tool_names_are_protocol_errors() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let result = client
        .call_tool(CallToolRequestParams::new("blender_eval"))
        .await;
    assert!(result.is_err(), "only scheme_eval exists: {result:?}");
    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_arguments_are_rejected_by_the_tool_schema() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("scheme_eval").with_arguments(
                json!({"code": "(+ 1 1)", "bogus": true})
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("the call itself completes");
    assert_eq!(
        result.is_error,
        Some(true),
        "deny_unknown_fields should reject the extra key"
    );
    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

#[tokio::test]
async fn timeouts_beyond_the_configured_maximum_are_refused() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let result = client
        .call_tool(
            CallToolRequestParams::new("scheme_eval").with_arguments(
                json!({"code": "(+ 1 1)", "timeout_secs": 86_400})
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("the call itself completes");
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["code"], "invalid_timeout");
    server_shutdown(client, server).await;
}

#[tokio::test]
async fn a_cpu_bound_loop_is_interrupted_and_the_engine_recovers() {
    let server = TestServer::start().await;
    let client = server.connect().await;

    let timed_out = client
        .call_tool(
            CallToolRequestParams::new("scheme_eval").with_arguments(
                json!({"code": "(define (spin) (spin)) (spin)", "timeout_secs": 1})
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("the timed-out call still returns a result");
    assert_eq!(timed_out.is_error, Some(true));
    assert_eq!(structured(&timed_out)["code"], "timeout");
    assert_eq!(structured(&timed_out)["timed_out"], json!(true));
    assert_eq!(
        structured(&timed_out)["potentially_continuing"],
        json!(false),
        "a pure CPU loop never dispatched Blender work"
    );

    // The same engine must keep serving requests after the watchdog fires.
    let recovered = evaluate(&server, "(+ 20 22)").await;
    assert_eq!(structured(&recovered)["result"], json!(42));
    server_shutdown(client, server).await;
}

#[tokio::test]
async fn concurrent_clients_are_serialized_in_fifo_order() {
    let server = TestServer::start().await;
    evaluate(&server, "(define call-log '())").await;

    let server = Arc::new(server);
    let mut handles = Vec::new();
    for index in 0..8 {
        let server = Arc::clone(&server);
        handles.push(tokio::spawn(async move {
            let result = evaluate(
                &server,
                &format!("(begin (set! call-log (cons {index} call-log)) (length call-log))"),
            )
            .await;
            let value = structured(&result);
            value["result"]
                .as_u64()
                .unwrap_or_else(|| panic!("call {index} did not return a length: {value}"))
        }));
    }

    let mut lengths = Vec::new();
    for handle in handles {
        lengths.push(handle.await.expect("task joins"));
    }
    lengths.sort_unstable();
    assert_eq!(
        lengths,
        (1..=8).collect::<Vec<u64>>(),
        "FIFO serialization must give every call a distinct list length"
    );

    Arc::try_unwrap(server)
        .unwrap_or_else(|_| panic!("server is still shared"))
        .shutdown()
        .await;
}

async fn server_shutdown(client: RunningService<RoleClient, ()>, server: TestServer) {
    client.cancel().await.expect("client closes");
    server.shutdown().await;
}

fn background_request(code: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("scheme_eval").with_arguments(
        json!({"code": code, "background": true})
            .as_object()
            .expect("object")
            .clone(),
    )
}

async fn start_task(client: &RunningService<RoleClient, ClientInfo>, code: &str) -> String {
    match client
        .call_tool_once(background_request(code))
        .await
        .expect("task starts")
    {
        CallToolResponse::Task(created) => created.task.task_id,
        response => panic!("expected a task handle: {response:?}"),
    }
}

async fn terminal_task(client: &RunningService<RoleClient, ClientInfo>, id: &str) -> DetailedTask {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = client
                .get_task(GetTaskParams::new(id))
                .await
                .expect("task exists")
                .task;
            if task.status().is_terminal() {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("task reaches terminal state")
}

#[tokio::test]
async fn background_requires_capability_without_executing_code() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let rejected = client
        .call_tool(background_request("(define unauthorized-task 42)"))
        .await
        .expect("tool-level rejection");
    assert_eq!(rejected.is_error, Some(true));
    assert_eq!(
        evaluate(&server, "unauthorized-task").await.is_error,
        Some(true)
    );
    server_shutdown(client, server).await;
}

#[tokio::test]
async fn background_survives_disconnect_and_preserves_tool_results() {
    let server = TestServer::start().await;
    let client = server.connect_tasks().await;
    let id = start_task(&client, "(begin (op-call \"test.slow\") 42)").await;
    client.cancel().await.expect("disconnect while task works");
    let reconnected = server.connect_tasks().await;
    let task = terminal_task(&reconnected, &id).await;
    let TaskPayload::Completed { result } = task.payload else {
        panic!("expected completed task: {task:?}");
    };
    let result: CallToolResult =
        serde_json::from_value(Value::Object(result)).expect("tool result");
    assert_eq!(structured(&result)["result"], 42);

    let id = start_task(&reconnected, "(op-call \"test.fail\")").await;
    let TaskPayload::Completed { result } = terminal_task(&reconnected, &id).await.payload else {
        panic!("tool failures are completed tasks with isError");
    };
    let result: CallToolResult = serde_json::from_value(Value::Object(result)).expect("tool error");
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["data"]["request_id"], 42);

    // Declaring capability alone does not make ordinary calls asynchronous.
    let response = reconnected
        .call_tool_once(
            CallToolRequestParams::new("scheme_eval").with_arguments(
                json!({"code": "(+ 1 2)"})
                    .as_object()
                    .expect("object")
                    .clone(),
            ),
        )
        .await
        .expect("synchronous response");
    assert!(matches!(response, CallToolResponse::Complete(_)));
    reconnected.cancel().await.expect("close client");
    server.shutdown().await;
}

#[tokio::test]
async fn cancelling_queued_task_prevents_its_mutation() {
    let server = TestServer::start().await;
    evaluate(&server, "(define task-mutation 0)").await;
    let client = server.connect_tasks().await;
    let blocker = start_task(&client, "(let loop () (loop))").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.worker.handle().status().state != "busy" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first task owns the worker");
    let queued = start_task(&client, "(set! task-mutation 1)").await;
    client
        .cancel_task(CancelTaskParams::new(&queued))
        .await
        .expect("cancel queued");
    assert_eq!(
        terminal_task(&client, &queued).await.status(),
        TaskStatus::Cancelled
    );
    client
        .cancel_task(CancelTaskParams::new(&blocker))
        .await
        .expect("cancel blocker");
    assert_eq!(
        terminal_task(&client, &blocker).await.status(),
        TaskStatus::Cancelled
    );
    assert_eq!(
        structured(&evaluate(&server, "task-mutation").await)["result"],
        0
    );
    client.cancel().await.expect("close client");
    server.shutdown().await;
}

#[tokio::test]
async fn task_admission_is_bounded_and_unknown_ids_are_rejected() {
    let server = TestServer::start().await;
    let client = server.connect_tasks().await;
    assert!(
        client
            .get_task(GetTaskParams::new("unknown"))
            .await
            .is_err()
    );
    assert!(
        client
            .cancel_task(CancelTaskParams::new("unknown"))
            .await
            .is_err()
    );
    assert!(
        client
            .update_task(
                serde_json::from_value::<UpdateTaskParams>(
                    json!({"taskId": "unknown", "inputResponses": {}})
                )
                .expect("update params")
            )
            .await
            .is_err()
    );
    for _ in 0..32 {
        let id = start_task(&client, "42").await;
        assert_eq!(
            terminal_task(&client, &id).await.status(),
            TaskStatus::Completed
        );
    }
    let error = client
        .call_tool_once(background_request("42"))
        .await
        .expect_err("retained capacity enforced");
    assert!(error.to_string().contains("task capacity"), "{error}");
    client.cancel().await.expect("close client");
    server.shutdown().await;
}
