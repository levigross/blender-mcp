use std::{future::ready, time::Duration};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ContentBlock, CreateTaskResult, GetTaskParams, GetTaskResult, Implementation,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ResourceContents,
        ServerCapabilities, ServerInfo, UpdateTaskParams,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    resources,
    scheme::SchemeHandle,
    sessions::{DEFAULT_SESSION, Sessions, artifact_uri},
    tasks::Tasks,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemeEvalParams {
    #[schemars(
        description = "Steel Scheme source to evaluate in the persistent Blender environment"
    )]
    pub code: String,
    #[schemars(
        description = "Named Blender session (default: default). Discover configured sessions at resources://blender/sessions. Clients selecting the same session share its Scheme state and scene."
    )]
    pub session: Option<String>,
    #[schemars(
        description = "Evaluation timeout in whole seconds (default 120, server maximum 3600)"
    )]
    pub timeout_secs: Option<u64>,
    #[schemars(description = "Rebuild the Steel environment before evaluating this code")]
    pub reset: Option<bool>,
    #[schemars(description = "Include Blender events produced during this evaluation")]
    pub include_events: Option<bool>,
    #[schemars(
        description = "Return an MCP task handle; requires the client to declare the tasks extension (default false)"
    )]
    pub background: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct BlenderMcp {
    sessions: Sessions,
    tool_router: ToolRouter<Self>,
    tasks: Tasks,
}

impl BlenderMcp {
    pub fn new(worker: SchemeHandle) -> Self {
        Self::with_sessions(Sessions::new(worker))
    }

    pub fn with_sessions(sessions: Sessions) -> Self {
        Self {
            sessions,
            tool_router: Self::tool_router(),
            tasks: Tasks::default(),
        }
    }
}

#[tool_router]
impl BlenderMcp {
    #[tool(
        name = "scheme_eval",
        description = "Evaluate Steel Scheme in the persistent Blender environment. All Blender operator and RNA access is available through Scheme bindings."
    )]
    async fn scheme_eval(
        &self,
        Parameters(parameters): Parameters<SchemeEvalParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let worker = self.sessions.get(parameters.session.as_deref())?;
        evaluate_scheme(worker, parameters, context.ct.clone()).await
    }
}

pub(crate) async fn evaluate_scheme(
    worker: &SchemeHandle,
    parameters: SchemeEvalParams,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<CallToolResult, McpError> {
    let timeout = parameters.timeout_secs.map(Duration::from_secs);
    let session = parameters.session.as_deref().unwrap_or(DEFAULT_SESSION);
    match worker
        .evaluate(
            parameters.code,
            timeout,
            parameters.reset.unwrap_or(false),
            parameters.include_events.unwrap_or(false),
            cancellation,
        )
        .await
    {
        Ok(reply) => {
            let mut structured = serde_json::to_value(&reply).map_err(|error| {
                McpError::internal_error(
                    "failed to serialize scheme_eval response",
                    Some(serde_json::json!({"reason": error.to_string()})),
                )
            })?;
            structured["session"] = serde_json::json!(session);
            if let Some(artifacts) = structured["artifacts"].as_array_mut() {
                for artifact in artifacts {
                    if let Some(id) = artifact["id"].as_str() {
                        artifact["uri"] = serde_json::json!(artifact_uri(session, id));
                    }
                }
            }
            let mut result = CallToolResult::structured(structured);
            result.content = vec![ContentBlock::text(reply.display.clone())];
            for artifact in reply.artifacts {
                if matches!(
                    artifact.mime_type.as_str(),
                    "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                ) {
                    result.content.push(ContentBlock::image(
                        artifact.data_base64,
                        artifact.mime_type,
                    ));
                } else {
                    result.content.push(ContentBlock::resource(
                        ResourceContents::blob(
                            artifact.data_base64,
                            artifact_uri(session, &artifact.id),
                        )
                        .with_mime_type(artifact.mime_type),
                    ));
                }
            }
            Ok(result)
        }
        Err(error) => {
            let mut structured = serde_json::to_value(&error).map_err(|serialization_error| {
                McpError::internal_error(
                    "failed to serialize scheme_eval error",
                    Some(serde_json::json!({"reason": serialization_error.to_string()})),
                )
            })?;
            structured["session"] = serde_json::json!(session);
            let mut result = CallToolResult::structured_error(structured);
            result.content = vec![ContentBlock::text(error.to_string())];
            Ok(result)
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlenderMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_tasks()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
        .with_server_info(
            Implementation::new("blender-mcp", env!("CARGO_PKG_VERSION"))
                .with_title("Scheme-first Blender MCP"),
        )
        .with_instructions(
            "Exactly one tool is exposed: scheme_eval. Read resources://blender for resource discovery, then blender-mcp://guide/getting-started and blender-mcp://reference/scheme. Read resources://blender/sessions to discover configured Blender sessions. Pass session to scheme_eval to select one; omission selects default. Clients selecting the same session share Scheme variables and Blender state, with serialized evaluations. Different sessions have independent workers and Blender instances. Reset affects only the selected Scheme environment. Use each returned artifact URI to read from the correct session. Start with (control-status) and (scene-summary); use RNA property/function metadata before unfamiliar edits. Clients declaring the tasks extension may pass background: true and poll tasks/get; see blender-mcp://guide/tasks. Tasks retain their selected session across reconnects. Use render-start and job-status/job-result in the same session for render jobs. On timeout inspect the request receipt before repeating a mutation. Reacquire handles after load/undo/restart. Check result_complete and display_truncated before treating output as complete."
                .to_owned(),
        )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name == "scheme_eval"
            && request
                .arguments
                .as_ref()
                .and_then(|args| args.get("background"))
                == Some(&serde_json::Value::Bool(true))
        {
            if !context
                .client_capabilities()
                .is_some_and(|caps| caps.supports_tasks())
            {
                return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                    ContentBlock::text(
                        "background evaluation requires the io.modelcontextprotocol/tasks client capability",
                    ),
                ])));
            }
            let parameters: SchemeEvalParams = serde_json::from_value(serde_json::Value::Object(
                request.arguments.unwrap_or_default(),
            ))
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            let worker = self.sessions.get(parameters.session.as_deref())?;
            return self
                .tasks
                .spawn(worker.clone(), parameters)
                .map(|task| CallToolResponse::Task(CreateTaskResult::new(task)));
        }
        self.tool_router
            .call(rmcp::handler::server::tool::ToolCallContext::new(
                self, request, context,
            ))
            .await
    }

    fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetTaskResult, McpError>> {
        ready(self.tasks.get(&request.task_id).map(GetTaskResult::new))
    }

    fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> {
        ready(self.tasks.cancel(&request.task_id))
    }

    fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> {
        ready(self.tasks.update(request))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, McpError>> {
        let mut result = resources::templates();
        if supports_cache_hints(&context) {
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        ready(Ok(result))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> {
        let mut result = ListToolsResult::with_all_items(self.tool_router.list_all());
        if supports_cache_hints(&context) {
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        ready(Ok(result))
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> {
        let mut result = resources::list_sessions(&self.sessions);
        if supports_cache_hints(&context) {
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        ready(Ok(result))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let mut result =
            resources::read_sessions(&request.uri, &self.sessions, context.ct.clone()).await?;
        if supports_cache_hints(&context) {
            // Contents include live session state and rendered artifacts.
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Private);
        }
        Ok(ReadResourceResponse::Complete(result))
    }
}

/// Protocol 2026-07-28 (SEP-2549) requires `ttlMs` and `cacheScope` on list and read results;
/// clients that negotiated it reject results missing them.
fn supports_cache_hints(context: &RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use async_trait::async_trait;
    use blender_mcp_protocol::{
        BridgeOperation, BridgeResponse, OperatorCatalog, PROTOCOL_VERSION,
    };
    use blender_mcp_transport::{BlenderBridge, BridgeHealth, BridgeMode, TransportError};
    use serde_json::json;

    use crate::scheme::{SchemeSettings, SchemeWorker};

    use super::*;

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
                BridgeOperation::Status => json!({"ok": true}),
                _ => json!(null),
            };
            Ok(BridgeResponse::success(1, value))
        }

        async fn health(&self) -> BridgeHealth {
            BridgeHealth {
                mode: BridgeMode::Live,
                address: "127.0.0.1:9876".parse().expect("address"),
                connected: true,
                process_running: None,
                recent_logs: Vec::new(),
            }
        }
    }

    async fn test_server() -> (BlenderMcp, SchemeWorker) {
        let catalog = OperatorCatalog {
            protocol_version: PROTOCOL_VERSION,
            revision: "test".to_owned(),
            blender_version: "test".to_owned(),
            operators: Vec::new(),
        };
        let worker = SchemeWorker::spawn(
            Arc::new(MockBridge),
            catalog,
            tokio::runtime::Handle::current(),
            SchemeSettings {
                default_timeout: Duration::from_secs(2),
                maximum_timeout: Duration::from_secs(10),
            },
        )
        .await
        .expect("worker");
        (BlenderMcp::new(worker.handle()), worker)
    }

    #[tokio::test]
    async fn tool_surface_contains_exactly_scheme_eval() {
        let (server, worker) = test_server().await;
        let names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<HashSet<_>>();
        assert_eq!(names, HashSet::from(["scheme_eval".to_owned()]));
        drop(server);
        worker.shutdown().await;
    }

    #[tokio::test]
    async fn scheme_definitions_persist_and_reset() {
        let (server, worker) = test_server().await;
        let cancellation = tokio_util::sync::CancellationToken::new();
        server
            .sessions
            .default_worker()
            .evaluate(
                "(define answer 42) answer".to_owned(),
                None,
                false,
                false,
                cancellation.clone(),
            )
            .await
            .expect("define");
        let persisted = server
            .sessions
            .default_worker()
            .evaluate(
                "answer".to_owned(),
                None,
                false,
                false,
                cancellation.clone(),
            )
            .await
            .expect("persisted");
        assert_eq!(persisted.result, json!(42));
        let reset = server
            .sessions
            .default_worker()
            .evaluate("(+ 1 2)".to_owned(), None, true, false, cancellation)
            .await
            .expect("reset eval");
        assert_eq!(reset.result, json!(3));
        drop(server);
        worker.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_before_a_task_is_polled_does_not_enqueue_it() {
        let (server, worker) = test_server().await;
        let task = server
            .tasks
            .spawn(
                server.sessions.default_worker().clone(),
                SchemeEvalParams {
                    code: "(define cancelled-task-mutation 42)".to_owned(),
                    session: None,
                    timeout_secs: None,
                    reset: None,
                    include_events: None,
                    background: Some(true),
                },
            )
            .expect("task admitted");
        // No await between admission and cancellation: the current-thread
        // runtime cannot have polled the newly spawned task yet.
        server
            .tasks
            .cancel(&task.task_id)
            .expect("cancellation accepted");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = server.tasks.get(&task.task_id).expect("task exists");
                if task.status().is_terminal() {
                    assert_eq!(task.status(), rmcp::model::TaskStatus::Cancelled);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled task settles");
        assert!(
            server
                .sessions
                .default_worker()
                .evaluate(
                    "cancelled-task-mutation".to_owned(),
                    None,
                    false,
                    false,
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .is_err()
        );
        drop(server);
        worker.shutdown().await;
    }
}
