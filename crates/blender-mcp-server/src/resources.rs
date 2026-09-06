use rmcp::{
    ErrorData as McpError,
    model::{
        ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult, Resource,
        ResourceContents, ResourceTemplate,
    },
};

use crate::scheme::SchemeHandle;

const MIME: &str = "text/markdown";

struct Entry {
    uri: &'static str,
    name: &'static str,
    description: &'static str,
    content: &'static str,
}

const ENTRIES: &[Entry] = &[
    Entry {
        uri: "resources://blender",
        name: "blender-resources",
        description: "Start here: Blender guides, references, tasks, runtime information, and artifacts",
        content: include_str!("../../../docs/resources.md"),
    },
    Entry {
        uri: "blender-mcp://guide/getting-started",
        name: "getting-started",
        description: "Install, start, and make the first Scheme calls",
        content: include_str!("../../../docs/getting-started.md"),
    },
    Entry {
        uri: "blender-mcp://guide/live",
        name: "live-mode",
        description: "Control a user-steerable Blender session",
        content: include_str!("../../../docs/live-mode.md"),
    },
    Entry {
        uri: "blender-mcp://guide/headless",
        name: "headless-mode",
        description: "Run a server-managed background Blender process",
        content: include_str!("../../../docs/headless-mode.md"),
    },
    Entry {
        uri: "blender-mcp://reference/scheme",
        name: "scheme-reference",
        description: "Steel functions for operators, RNA, rendering, and recovery",
        content: include_str!("../../../docs/scheme-reference.md"),
    },
    Entry {
        uri: "blender-mcp://reference/stdlib",
        name: "stdlib",
        description: "Built-in helpers: maths, lookup, creation, transforms, nodes, rendering",
        content: include_str!("../../../docs/stdlib.md"),
    },
    Entry {
        uri: "blender-mcp://reference/steel",
        name: "steel-reference",
        description: "The Steel Scheme language: forms, data, errors, and sandbox limits",
        content: include_str!("../../../docs/steel-reference.md"),
    },
    Entry {
        uri: "blender-mcp://reference/rna",
        name: "rna-reference",
        description: "RNA roots, selectors, references, calls, and limits",
        content: include_str!("../../../docs/rna-reference.md"),
    },
    Entry {
        uri: "blender-mcp://reference/blender-api",
        name: "blender-api",
        description: "Mapping bpy onto the RNA bindings: collections, None, node trees, cost",
        content: include_str!("../../../docs/blender-api.md"),
    },
    Entry {
        uri: "blender-mcp://guide/security",
        name: "security",
        description: "Sandbox boundaries, trusted Blender authority, and HTTP exposure",
        content: include_str!("../../../docs/security.md"),
    },
    Entry {
        uri: "blender-mcp://recipes",
        name: "recipes",
        description: "End-to-end discovery, modeling, rendering, and recovery examples",
        content: include_str!("../../../docs/recipes.md"),
    },
    Entry {
        uri: "blender-mcp://guide/tasks",
        name: "tasks",
        description: "Background MCP evaluations, result polling, cancellation, and retention",
        content: include_str!("../../../docs/tasks.md"),
    },
];

pub fn list(worker: &SchemeHandle) -> ListResourcesResult {
    let mut resources = ENTRIES
        .iter()
        .map(|entry| {
            Resource::new(entry.uri, entry.name)
                .with_description(entry.description)
                .with_mime_type(MIME)
                .with_size(u64::try_from(entry.content.len()).unwrap_or(u64::MAX))
        })
        .collect::<Vec<_>>();
    let catalog = worker.catalog();
    resources.push(
        Resource::new("blender-mcp://runtime/catalog", "runtime-catalog")
            .with_description("Active Blender version, operator count, and catalog revision")
            .with_mime_type(MIME)
            .with_size(u64::try_from(runtime_catalog(&catalog).len()).unwrap_or(u64::MAX)),
    );
    resources.push(
        Resource::new("blender-mcp://runtime/status", "runtime-status")
            .with_description(
                "Local Scheme worker state and queued evaluations; does not contact Blender",
            )
            .with_mime_type("application/json"),
    );
    ListResourcesResult::with_all_items(resources)
}

pub fn templates() -> ListResourceTemplatesResult {
    ListResourceTemplatesResult::with_all_items(vec![
        ResourceTemplate::new("blender-mcp://artifact/{id}", "artifact")
            .with_description("Read an existing render artifact by its returned ID; MIME type comes from the artifact"),
        ResourceTemplate::new("resources://blender/artifact/{id}", "artifact-alias")
            .with_description("Alias for an existing render artifact; MIME type comes from the artifact"),
    ])
}

pub async fn read(
    uri: &str,
    worker: &SchemeHandle,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<ReadResourceResult, McpError> {
    let canonical = uri
        .strip_prefix("resources://blender/")
        .map(|suffix| format!("blender-mcp://{suffix}"));
    let lookup_uri = canonical.as_deref().unwrap_or(uri);
    if let Some(id) = lookup_uri.strip_prefix("blender-mcp://artifact/") {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(McpError::invalid_params("invalid artifact ID", None));
        }
        let value = worker
            .artifact(id.to_owned(), cancellation)
            .await
            .map_err(|error| {
                McpError::invalid_params(error.to_string(), error.structured_data())
            })?;
        let data = value["data_base64"]
            .as_str()
            .ok_or_else(|| McpError::internal_error("artifact payload missing", None))?;
        let mime = value["artifact"]["mime_type"]
            .as_str()
            .ok_or_else(|| McpError::internal_error("artifact MIME missing", None))?;
        return Ok(ReadResourceResult::new(vec![
            ResourceContents::blob(data, uri).with_mime_type(mime),
        ]));
    }
    if lookup_uri == "blender-mcp://runtime/status" {
        let text = serde_json::to_string_pretty(&worker.status())
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        return Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, uri).with_mime_type("application/json"),
        ]));
    }
    let text = if lookup_uri == "blender-mcp://runtime/catalog" {
        runtime_catalog(&worker.catalog())
    } else {
        ENTRIES
            .iter()
            .find(|entry| entry.uri == lookup_uri)
            .map(|entry| entry.content.to_owned())
            .ok_or_else(|| McpError::invalid_params(format!("unknown resource URI: {uri}"), None))?
    };
    Ok(ReadResourceResult::new(vec![
        ResourceContents::text(text, uri).with_mime_type(MIME),
    ]))
}

fn runtime_catalog(catalog: &blender_mcp_protocol::OperatorCatalog) -> String {
    format!(
        "# Runtime Blender catalog\n\n- Blender: `{}`\n- Operators: `{}`\n- Revision: `{}`\n- Protocol: `{}`\n\nUse `(operators)`, `(operator-search query)`, and `(operator-info idname)` through `scheme_eval` for complete metadata.\n",
        catalog.blender_version,
        catalog.operators.len(),
        catalog.revision,
        catalog.protocol_version,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn resource_uris_are_unique() {
        let mut uris = HashSet::new();
        for entry in ENTRIES {
            assert!(uris.insert(entry.uri), "duplicate URI: {}", entry.uri);
        }
        assert!(uris.insert("blender-mcp://runtime/catalog"));
        assert!(uris.insert("blender-mcp://runtime/status"));
    }
}
