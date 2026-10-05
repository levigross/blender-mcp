//! Named sessions over the public MCP transport: shared clients, isolated workers,
//! session-scoped resources, and background evaluations.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use blender_mcp_protocol::{BridgeOperation, BridgeResponse, OperatorCatalog, PROTOCOL_VERSION};
use blender_mcp_server::{
    http,
    scheme::{SchemeSettings, SchemeWorker},
    server::BlenderMcp,
    sessions::Sessions,
};
use blender_mcp_transport::{BlenderBridge, BridgeHealth, BridgeMode, TransportError};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt as _, ServiceExt as _,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ClientCapabilities, ClientInfo, DetailedTask, GetTaskParams, Implementation,
        ProtocolVersion, ReadResourceRequestParams, ResourceContents, TaskPayload, TaskStatus,
    },
    service::{RoleClient, RunningService},
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct MockBridge {
    name: &'static str,
    artifact: &'static str,
    entered: Notify,
    release: Semaphore,
    /// `ReferenceReset` requests received: a reset must reach only its own Blender.
    resets: std::sync::atomic::AtomicUsize,
}

impl MockBridge {
    fn new(name: &'static str, artifact: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            artifact,
            entered: Notify::new(),
            release: Semaphore::new(0),
            resets: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl BlenderBridge for MockBridge {
    async fn request(
        &self,
        operation: BridgeOperation,
        _request_timeout: Duration,
    ) -> Result<BridgeResponse, TransportError> {
        let value = match operation {
            BridgeOperation::Status => json!({"instance": self.name}),
            BridgeOperation::ReferenceReset => {
                self.resets
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                json!({"generation": 2})
            }
            BridgeOperation::OperatorCall { idname, .. } if idname == "test.block" => {
                self.entered.notify_one();
                self.release.acquire().await.expect("gate open").forget();
                json!(self.name)
            }
            BridgeOperation::Artifact {
                artifact_id,
                include_data,
            } => {
                assert_eq!(artifact_id, "frame");
                assert!(include_data);
                json!({
                    "artifact": {"id": "frame", "mime_type": "image/png"},
                    "data_base64": self.artifact,
                })
            }
            operation => panic!("unexpected mock bridge operation: {operation:?}"),
        };
        Ok(BridgeResponse::success(1, value))
    }

    async fn request_cancellable(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<BridgeResponse, TransportError> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(TransportError::Blender {
                code: "cancelled".to_owned(),
                message: "mock operation cancelled before its effect".to_owned(),
                potentially_continuing: false,
                data: None,
                retryable: false,
                request_id: 1,
                session_id: self.name.to_owned(),
                instance_id: Some(self.name.to_owned()),
            }),
            result = self.request(operation, request_timeout) => result,
        }
    }

    async fn health(&self) -> BridgeHealth {
        BridgeHealth {
            mode: BridgeMode::Headless,
            address: "127.0.0.1:9876".parse().expect("mock address"),
            connected: true,
            process_running: Some(true),
            recent_logs: Vec::new(),
        }
    }
}

struct TestServer {
    address: SocketAddr,
    workers: Vec<SchemeWorker>,
    primary: Arc<MockBridge>,
    alternate: Arc<MockBridge>,
    shutdown: CancellationToken,
    serve: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let primary = MockBridge::new("default", "ZGVmYXVsdA==");
        let alternate = MockBridge::new("alternate", "YWx0ZXJuYXRl");
        let mut workers = Vec::new();
        for bridge in [&primary, &alternate] {
            workers.push(
                SchemeWorker::spawn(
                    bridge.clone(),
                    OperatorCatalog {
                        protocol_version: PROTOCOL_VERSION,
                        revision: format!("{}-catalog", bridge.name),
                        blender_version: "5.2.1".to_owned(),
                        operators: Vec::new(),
                    },
                    tokio::runtime::Handle::current(),
                    SchemeSettings {
                        default_timeout: Duration::from_secs(10),
                        maximum_timeout: Duration::from_secs(30),
                    },
                )
                .await
                .expect("worker starts"),
            );
        }
        let mut sessions = Sessions::new(workers[0].handle());
        sessions
            .insert("alternate".to_owned(), workers[1].handle())
            .expect("register second session");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("bound address");
        let shutdown = CancellationToken::new();
        let router = http::router(
            BlenderMcp::with_sessions(sessions),
            primary.clone(),
            workers[0].handle(),
            http::loopback_policy(address, None),
            &shutdown,
            http::DEFAULT_MAX_MCP_BODY_BYTES,
        );
        let serve_shutdown = shutdown.clone();
        let serve = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
                .await
                .expect("HTTP service runs");
        });
        Self {
            address,
            workers,
            primary,
            alternate,
            shutdown,
            serve,
        }
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        ().serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{}/mcp",
            self.address
        )))
        .await
        .expect("client connects")
    }

    async fn evaluate(&self, args: Value) -> CallToolResult {
        let client = self.connect().await;
        let result = client
            .call_tool(request(args))
            .await
            .expect("tool responds");
        client.cancel().await.expect("close client");
        result
    }

    async fn connect_tasks(&self) -> RunningService<RoleClient, ClientInfo> {
        ClientInfo::new(
            ClientCapabilities::builder().enable_tasks().build(),
            Implementation::new("multiplex-test", "1"),
        )
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_uri(format!("http://{}/mcp", self.address)),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("task-capable client")
    }

    async fn shutdown(self) {
        self.primary.release.add_permits(1);
        self.alternate.release.add_permits(1);
        self.shutdown.cancel();
        drop(self.serve.await);
        for worker in self.workers {
            worker.shutdown().await;
        }
    }
}

fn request(args: Value) -> CallToolRequestParams {
    let Value::Object(arguments) = args else {
        panic!("expected argument object");
    };
    CallToolRequestParams::new("scheme_eval").with_arguments(arguments)
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("structured result")
}

fn text(contents: &ResourceContents) -> &str {
    let ResourceContents::TextResourceContents { text, .. } = contents else {
        panic!("expected text resource");
    };
    text
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
                break task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("task completes")
}

struct ReleaseGate(Arc<MockBridge>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release.add_permits(1);
    }
}

#[tokio::test]
async fn clients_share_selected_state_and_reset_does_not_cross_sessions() {
    let server = TestServer::start().await;
    server.evaluate(json!({"code": "(define counter 7)"})).await;
    server
        .evaluate(json!({"session": "alternate", "code": "(define counter 41)"}))
        .await;
    let observed = server
        .evaluate(json!({
            "session": "alternate",
            "code": "(begin (set! counter (+ counter 1)) counter)",
        }))
        .await;
    assert_eq!(structured(&observed)["session"], "alternate");
    assert_eq!(structured(&observed)["result"], 42);
    let primary = server.evaluate(json!({"code": "counter"})).await;
    assert_eq!(structured(&primary)["session"], "default");
    assert_eq!(structured(&primary)["result"], 7);
    let reset = server
        .evaluate(json!({"session": "alternate", "code": "0", "reset": true}))
        .await;
    assert_ne!(reset.is_error, Some(true));
    let cleared = server
        .evaluate(json!({"session": "alternate", "code": "counter"}))
        .await;
    assert_eq!(cleared.is_error, Some(true));
    assert_eq!(structured(&cleared)["session"], "alternate");
    let preserved = server
        .evaluate(json!({"session": "default", "code": "counter"}))
        .await;
    assert_eq!(structured(&preserved)["result"], 7);
    let resets = |bridge: &MockBridge| bridge.resets.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!((resets(&server.primary), resets(&server.alternate)), (0, 1));
    server.shutdown().await;
}

#[tokio::test]
async fn a_blocked_blender_does_not_block_another_session() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let blocked = tokio::spawn(async move {
        let result = client
            .call_tool(request(json!({"code": "(op-call \"test.block\")"})))
            .await;
        client.cancel().await.expect("close blocking client");
        result
    });
    let entered =
        tokio::time::timeout(Duration::from_secs(5), server.primary.entered.notified()).await;
    let other = tokio::time::timeout(
        Duration::from_secs(3),
        server.evaluate(json!({"session": "alternate", "code": "(blender-status)"})),
    )
    .await;
    let still_blocked = !blocked.is_finished();
    // Release the native callback before asserting, including timeout failures.
    server.primary.release.add_permits(1);
    let resumed = blocked
        .await
        .expect("blocking client joins")
        .expect("tool result");
    server.shutdown().await;
    assert!(entered.is_ok(), "default callback entered its gate");
    assert!(still_blocked, "default evaluation stayed blocked");
    let other = other.expect("second Blender responds while the first is blocked");
    assert_eq!(structured(&other)["result"]["instance"], "alternate");
    assert_eq!(structured(&other)["session"], "alternate");
    assert_eq!(structured(&resumed)["result"], "default");
}

#[tokio::test]
async fn session_resources_route_catalogs_and_colliding_artifact_ids() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let listed = client.list_resources(None).await.expect("resources/list");
    assert!(
        listed
            .resources
            .iter()
            .any(|resource| resource.uri == "resources://blender/sessions")
    );
    let index = client
        .read_resource(ReadResourceRequestParams::new(
            "resources://blender/sessions",
        ))
        .await
        .expect("session index");
    let index: Value = serde_json::from_str(text(&index.contents[0])).expect("index JSON");
    assert_eq!(index["default_session"], "default");
    let names = index["sessions"].as_array().expect("session array");
    assert_eq!(names.len(), 2);
    for (name, bytes) in [("default", "ZGVmYXVsdA=="), ("alternate", "YWx0ZXJuYXRl")] {
        assert!(names.iter().any(|entry| entry["name"] == name));
        for (suffix, expected) in [
            ("runtime/catalog", format!("{name}-catalog")),
            ("runtime/status", "ready".to_owned()),
        ] {
            let uri = format!("resources://blender/sessions/{name}/{suffix}");
            let result = client
                .read_resource(ReadResourceRequestParams::new(&uri))
                .await
                .expect("session runtime resource");
            assert!(text(&result.contents[0]).contains(&expected));
        }
        let uri = format!("resources://blender/sessions/{name}/artifact/frame");
        let artifact = client
            .read_resource(ReadResourceRequestParams::new(&uri))
            .await
            .expect("session artifact");
        assert_eq!(
            artifact.contents,
            vec![ResourceContents::blob(bytes, &uri).with_mime_type("image/png")]
        );
    }
    for prefix in ["blender-mcp://", "resources://blender/"] {
        let uri = format!("{prefix}artifact/frame");
        let artifact = client
            .read_resource(ReadResourceRequestParams::new(&uri))
            .await
            .expect("legacy default artifact");
        assert_eq!(
            artifact.contents,
            vec![ResourceContents::blob("ZGVmYXVsdA==", &uri).with_mime_type("image/png")]
        );
    }
    for uri in [
        "resources://blender/sessions/missing/artifact/frame",
        "resources://blender/sessions/missing/runtime/catalog",
        "resources://blender/sessions//artifact/frame",
        "resources://blender/sessions/alternate%2fdefault/artifact/frame",
    ] {
        assert!(
            client
                .read_resource(ReadResourceRequestParams::new(uri))
                .await
                .is_err(),
            "must not fall back to default: {uri}"
        );
    }
    client.cancel().await.expect("close resource client");
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_sessions_reject_mutations_without_default_fallback() {
    let server = TestServer::start().await;
    let client = server.connect().await;
    let result = client
        .call_tool(request(json!({
            "session": "missing",
            "code": "(define unexpected-mutation 42)",
        })))
        .await;
    assert!(
        result.is_err()
            || result
                .as_ref()
                .is_ok_and(|value| value.is_error == Some(true))
    );
    client.cancel().await.expect("close client");
    let untouched = server
        .evaluate(json!({"code": "unexpected-mutation"}))
        .await;
    assert_eq!(untouched.is_error, Some(true));
    server.shutdown().await;
}

#[tokio::test]
async fn background_tasks_retain_the_selected_session_after_disconnect() {
    let server = TestServer::start().await;
    let client = server.connect_tasks().await;
    let response = client
        .call_tool_once(request(json!({
            "session": "alternate",
            "background": true,
            "code": "(begin (define task-value 42) (blender-status))",
        })))
        .await
        .expect("background admission");
    let CallToolResponse::Task(created) = response else {
        panic!("expected background task");
    };
    let id = created.task.task_id;
    client.cancel().await.expect("disconnect submitting client");
    let client = server.connect_tasks().await;
    let task = terminal_task(&client, &id).await;
    let TaskPayload::Completed { result } = task.payload else {
        panic!("expected completed task");
    };
    let result: CallToolResult =
        serde_json::from_value(Value::Object(result)).expect("tool result");
    assert_eq!(structured(&result)["session"], "alternate");
    assert_eq!(structured(&result)["result"]["instance"], "alternate");
    let isolated = server.evaluate(json!({"code": "task-value"})).await;
    assert_eq!(isolated.is_error, Some(true));
    let selected = server
        .evaluate(json!({"session": "alternate", "code": "task-value"}))
        .await;
    assert_eq!(structured(&selected)["result"], 42);
    client.cancel().await.expect("close task poller");
    server.shutdown().await;
}

#[tokio::test]
async fn cancelling_busy_and_queued_tasks_is_scoped_to_the_selected_worker() {
    let server = TestServer::start().await;
    let release = ReleaseGate(server.alternate.clone());
    for session in ["default", "alternate"] {
        let result = server
            .evaluate(json!({"session": session, "code": "(define task-mutation 0)"}))
            .await;
        assert_ne!(result.is_error, Some(true));
    }
    let client = server.connect_tasks().await;
    let busy = client
        .call_tool_once(request(json!({
            "session": "alternate",
            "background": true,
            "code": "(begin (op-call \"test.block\") (set! task-mutation 1))",
        })))
        .await
        .expect("busy task admitted");
    let CallToolResponse::Task(busy) = busy else {
        panic!("expected busy task handle");
    };
    tokio::time::timeout(Duration::from_secs(5), server.alternate.entered.notified())
        .await
        .expect("alternate callback owns the worker");
    let queued = client
        .call_tool_once(request(json!({
            "session": "alternate",
            "background": true,
            "code": "(set! task-mutation 2)",
        })))
        .await
        .expect("queued task admitted");
    let CallToolResponse::Task(queued) = queued else {
        panic!("expected queued task handle");
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.workers[1].handle().status().queued == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second task reached the alternate worker queue");

    let primary = tokio::time::timeout(
        Duration::from_secs(3),
        server.evaluate(json!({"code": "task-mutation"})),
    )
    .await
    .expect("default evaluates while alternate is busy and queued");
    assert_eq!(structured(&primary)["session"], "default");
    assert_eq!(structured(&primary)["result"], 0);
    client
        .cancel_task(CancelTaskParams::new(&queued.task.task_id))
        .await
        .expect("cancel queued task before releasing the blocker");
    assert_eq!(
        terminal_task(&client, &queued.task.task_id).await.status(),
        TaskStatus::Cancelled
    );
    client
        .cancel_task(CancelTaskParams::new(&busy.task.task_id))
        .await
        .expect("cancel busy task before releasing its callback");
    assert_eq!(
        terminal_task(&client, &busy.task.task_id).await.status(),
        TaskStatus::Cancelled
    );
    drop(release);

    for session in ["default", "alternate"] {
        let result = server
            .evaluate(json!({"session": session, "code": "task-mutation"}))
            .await;
        assert_ne!(result.is_error, Some(true));
        assert_eq!(structured(&result)["session"], session);
        assert_eq!(structured(&result)["result"], 0);
    }
    client.cancel().await.expect("close cancellation client");
    server.shutdown().await;
}
