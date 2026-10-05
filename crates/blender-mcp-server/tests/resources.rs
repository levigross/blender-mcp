use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use blender_mcp_protocol::{BridgeOperation, BridgeResponse, OperatorCatalog, PROTOCOL_VERSION};
use blender_mcp_server::{
    resources,
    scheme::{SchemeSettings, SchemeWorker},
    sessions::Sessions,
};
use blender_mcp_transport::{BlenderBridge, BridgeHealth, BridgeMode, TransportError};
use rmcp::model::{ErrorCode, ResourceContents};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct ArtifactOnlyBridge;

#[async_trait]
impl BlenderBridge for ArtifactOnlyBridge {
    async fn request(
        &self,
        operation: BridgeOperation,
        _request_timeout: Duration,
    ) -> Result<BridgeResponse, TransportError> {
        let BridgeOperation::Artifact {
            artifact_id,
            include_data,
        } = operation
        else {
            panic!("resource read must not run Blender operations: {operation:?}");
        };
        assert_eq!(artifact_id, "frame-1");
        assert!(include_data);
        Ok(BridgeResponse::success(
            1,
            json!({
                "artifact": {"id": artifact_id, "mime_type": "image/png"},
                "data_base64": "aW1hZ2U="
            }),
        ))
    }

    async fn health(&self) -> BridgeHealth {
        BridgeHealth {
            mode: BridgeMode::Headless,
            address: "127.0.0.1:9876".parse().unwrap(),
            connected: false,
            process_running: Some(false),
            recent_logs: Vec::new(),
        }
    }
}

async fn worker() -> SchemeWorker {
    SchemeWorker::spawn(
        Arc::new(ArtifactOnlyBridge),
        OperatorCatalog {
            protocol_version: PROTOCOL_VERSION,
            revision: "resource-test".to_owned(),
            blender_version: "5.2.1".to_owned(),
            operators: Vec::new(),
        },
        tokio::runtime::Handle::current(),
        SchemeSettings {
            default_timeout: Duration::from_secs(5),
            maximum_timeout: Duration::from_secs(10),
            library: None,
        },
    )
    .await
    .expect("worker starts without Blender")
}

fn text(contents: &ResourceContents) -> (&str, &str, &str) {
    match contents {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => (uri, mime_type.as_deref().expect("MIME type"), text),
        _ => panic!("expected text resource"),
    }
}

#[tokio::test]
async fn index_and_legacy_resources_are_discoverable_and_aliases_match() {
    let worker = worker().await;
    let handle = worker.handle();
    let listed = resources::list(&handle).resources;
    for uri in [
        "resources://blender",
        "blender-mcp://guide/getting-started",
        "blender-mcp://guide/live",
        "blender-mcp://guide/headless",
        "blender-mcp://guide/tasks",
        "blender-mcp://guide/security",
        "blender-mcp://reference/scheme",
        "blender-mcp://reference/stdlib",
        "blender-mcp://reference/steel",
        "blender-mcp://reference/rna",
        "blender-mcp://reference/blender-api",
        "blender-mcp://recipes",
        "blender-mcp://runtime/catalog",
        "blender-mcp://runtime/status",
    ] {
        assert!(listed.iter().any(|entry| entry.uri == uri), "missing {uri}");
    }
    let index = resources::read("resources://blender", &handle, CancellationToken::new())
        .await
        .unwrap();
    let (_, mime, index_text) = text(&index.contents[0]);
    assert_eq!(mime, "text/markdown");
    for entry in &listed {
        let Some(suffix) = entry.uri.strip_prefix("blender-mcp://") else {
            continue;
        };
        let alias = format!("resources://blender/{suffix}");
        assert!(index_text.contains(&alias), "index missing {alias}");
        let original = resources::read(&entry.uri, &handle, CancellationToken::new())
            .await
            .unwrap();
        let aliased = resources::read(&alias, &handle, CancellationToken::new())
            .await
            .unwrap();
        let (returned_uri, mime, contents) = text(&aliased.contents[0]);
        let (_, original_mime, original_contents) = text(&original.contents[0]);
        assert_eq!(returned_uri, alias);
        assert_eq!(mime, original_mime);
        assert_eq!(contents, original_contents);
        assert_eq!(entry.mime_type.as_deref(), Some(mime));
    }
    worker.shutdown().await;
}

#[tokio::test]
async fn status_is_a_local_json_snapshot_without_blender_requests() {
    let worker = worker().await;
    let result = resources::read(
        "resources://blender/runtime/status",
        &worker.handle(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let (_, mime, contents) = text(&result.contents[0]);
    assert_eq!(mime, "application/json");
    let value: Value = serde_json::from_str(contents).unwrap();
    assert_eq!(value, json!({"state": "ready", "queued": 0}));
    worker.shutdown().await;
}

#[tokio::test]
async fn artifact_templates_expand_to_readable_blobs_with_actual_mime() {
    let worker = worker().await;
    let templates = resources::templates().resource_templates;
    let artifacts = templates
        .into_iter()
        .filter(|template| template.uri_template.ends_with("/artifact/{id}"))
        .collect::<Vec<_>>();
    assert_eq!(artifacts.len(), 3);
    let sessions = Sessions::new(worker.handle());
    for template in artifacts {
        assert!(template.mime_type.is_none(), "artifact MIME is not fixed");
        let uri = template
            .uri_template
            .replace("{id}", "frame-1")
            .replace("{session}", "default");
        let result = resources::read_sessions(&uri, &sessions, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.contents,
            vec![ResourceContents::blob("aW1hZ2U=", &uri).with_mime_type("image/png")]
        );
    }
    worker.shutdown().await;
}

#[tokio::test]
async fn unknown_uris_and_invalid_artifact_ids_fail_without_bridge_access() {
    let worker = worker().await;
    let mut uris = vec![
        "resources://blender/unknown".to_owned(),
        "resources://other/guide/tasks".to_owned(),
        "blender-mcp://reference/rna?file=secret".to_owned(),
    ];
    for prefix in ["blender-mcp://artifact/", "resources://blender/artifact/"] {
        for id in ["", "../secret", "frame%2f1", "främé", "frame?data=true"] {
            uris.push(format!("{prefix}{id}"));
        }
        uris.push(format!("{prefix}{}", "a".repeat(129)));
    }
    for uri in uris {
        let error = resources::read(&uri, &worker.handle(), CancellationToken::new())
            .await
            .expect_err("invalid resource must fail");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "{uri}");
    }
    worker.shutdown().await;
}
