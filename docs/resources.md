# Blender resources

Read `resources://blender` for this index. MCP clients discover guides with
`resources/list` and read their contents with `resources/read`.

## Guides and references

| Resource URI | Purpose |
| --- | --- |
| `resources://blender/guide/getting-started` | Install, connect, and make the first Scheme calls |
| `resources://blender/guide/live` | Work with an interactive Blender session |
| `resources://blender/guide/headless` | Run a server-managed background Blender process |
| `resources://blender/guide/tasks` | Run background evaluations, poll results, and cancel tasks |
| `resources://blender/guide/sessions` | Share scenes between agents and select independent Blender instances |
| `resources://blender/guide/security` | Understand trust boundaries and HTTP exposure |
| `resources://blender/reference/scheme` | Discover available Scheme bindings |
| `resources://blender/reference/stdlib` | Use modeling, transform, node, and render helpers |
| `resources://blender/reference/steel` | Learn the Scheme language and sandbox limits |
| `resources://blender/reference/rna` | Inspect RNA properties, methods, and handle lifetimes |
| `resources://blender/reference/blender-api` | Map Blender's Python API to Scheme |
| `resources://blender/recipes` | Follow modeling, animation, rendering, and recovery examples |

Start with the getting-started and tasks guides. Before an unfamiliar edit, read
the RNA reference and inspect property or function metadata with `scheme_eval`.

## Runtime and artifacts

- `resources://blender/sessions`: JSON list of configured session names and local
  worker status. Each session also exposes
  `resources://blender/sessions/{session}/runtime/status`,
  `resources://blender/sessions/{session}/runtime/catalog`, and
  `resources://blender/sessions/{session}/artifact/{id}`. Read each returned
  artifact's `uri` to select the correct session. See [Sessions](sessions.md).
- `resources://blender/runtime/catalog`: Markdown summary of the cached Blender
  version, operator count, catalog revision, and protocol version.
- `resources://blender/runtime/status`: JSON snapshot containing the local Scheme
  worker `state` and number of `queued` evaluations. This does not contact Blender
  or evaluate Scheme. A ready worker does not establish that Blender is connected;
  use `(control-status)` through `scheme_eval` for session diagnostics.
- `resources://blender/artifact/{id}`: read an existing render artifact using the
  ID returned by Blender. `resources/templates/list` advertises this template.
  The resource contains a base64 blob with the artifact's MIME type. Reading an
  artifact does not start a render. IDs contain only ASCII letters, digits, and
  hyphens, with a maximum of 128 characters.

Unqualified runtime and artifact URIs address the `default` session.

Resource reads provide documentation, local runtime information, or existing
artifacts. Run Blender operations through `scheme_eval`; the task guide describes
background evaluations and their relationship to render jobs.

## URI compatibility

Every `resources://blender/<suffix>` URI above also works as
`blender-mcp://<suffix>`. Existing `blender-mcp://` discovery entries remain listed
for compatibility, alongside this index and the new task and status entries.
Read responses retain the URI requested by the client. Documentation resources
use `text/markdown`, worker status uses `application/json`, and artifact MIME
types depend on their contents.

URIs are exact and case-sensitive. Unknown resources and invalid artifact IDs
return an MCP invalid-parameters error. This is a fixed resource catalog, not a
filesystem browser; query strings and arbitrary paths are not supported.
