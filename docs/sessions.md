# Shared and independent Blender sessions

One MCP endpoint can serve multiple agents and multiple Blender instances.
`scheme_eval` remains the only tool. Its optional `session` argument selects a
named Blender session; omitting it selects `default`.

## Sharing a scene

Agents using the same session share its Blender scene and persistent Scheme
definitions. Evaluations enter that session's bounded FIFO queue and execute one
at a time. A single evaluation can contain several expressions to keep a sequence
of edits together. Separate calls from different agents can interleave.

```json
{
  "name": "scheme_eval",
  "arguments": {
    "session": "default",
    "code": "(define shared-cube (add-cube))"
  }
}
```

Another client can reference `shared-cube` in that session. Clients must
coordinate object ownership, selection, and naming. Serialization does not provide
transactions or roll back failed edits. Running render jobs and external edits
through Blender's UI retain their existing concurrency and cancellation rules.

## Configuring independent sessions

The normal command-line options configure `default`. Add `--sessions-file` to
load a JSON array of additional sessions at startup. For example, save this as
`sessions.json`:

```json
[
  {"name": "assets", "backend": "headless"},
  {"name": "level", "backend": "headless", "blend_file": "scenes/level.blend"},
  {"name": "editor", "backend": "live", "bridge": "127.0.0.1:9877"}
]
```

```sh
nix run github:levigross/blender-mcp#blender-mcp-cpu -- \
  --backend headless --sessions-file ./sessions.json
```

Create the referenced `.blend` file and start the live bridge before using those
entries, or omit them to start with only the `assets` session. Each live Blender
instance must expose its bridge on a distinct loopback address. Headless sessions
receive separate Blender processes and automatically allocated bridge ports.

Each session gets a persistent Steel worker, Scheme definitions, queue, Blender
scene, RNA handles, and artifact store. Evaluations in different sessions can
progress concurrently. Resources such as CPU, GPU, memory, and filesystem paths
are still shared by these processes; use distinct output paths for concurrent
renders. Sessions are work contexts for trusted callers, not tenant sandboxes.

Headless entries inherit the packaged executable, extension, startup timeout,
and other runtime settings. A relative `blend_file` is resolved against the
sessions file's directory. Extra sessions do not inherit `--blend-file` from
`default`. Names contain 1–64 ASCII letters, digits, underscores, or hyphens.
Names are case-sensitive, `default` is reserved, and at most 16 sessions may be
configured, including the default. Invalid configuration fails startup.

Session configuration is fixed until restart. On shutdown the server stops its
workers and owned headless processes; live Blender applications remain open.
Keep this JSON alongside the consuming project's flake and scene configuration.

## Selecting and discovering sessions

Read `resources://blender/sessions` for configured names and local worker status.
The status snapshot does not contact Blender. For current Blender diagnostics,
evaluate `(control-status)` in the selected session.

```json
{
  "name": "scheme_eval",
  "arguments": {"session": "assets", "code": "(scene-summary)"}
}
```

Tool results identify the selected session in `structuredContent.session`.
An unknown name fails the request without falling back to another session.
`reset: true` rebuilds only the selected Scheme environment; it does not reset
that Blender scene. A reset is visible to all agents sharing that session.
RNA handles, render job IDs, and request receipts belong to their originating
session. Use the same session when inspecting or cancelling its work.

`background: true` uses the selected worker and survives client disconnects.
Poll or cancel the returned task ID through the normal MCP task methods. The
task remains bound to its original session; reconnecting does not change it.
The 32-record MCP task limit is shared across the endpoint.

## Session resources

- `resources://blender/sessions/{session}/runtime/status`: local worker JSON.
- `resources://blender/sessions/{session}/runtime/catalog`: cached Blender catalog summary.
- `resources://blender/sessions/{session}/artifact/{id}`: artifact bytes from that session.

Each item in `structuredContent.artifacts` includes a `uri` for resource reads.
Use that URI, since artifact IDs alone do not select a session. Legacy runtime
and artifact URIs continue to address `default`. Scoped URIs also accept the
`blender-mcp://sessions/` prefix. The HTTP `/healthz` endpoint continues to
describe `default`; inspect named session resources and `(control-status)` for
the others.
