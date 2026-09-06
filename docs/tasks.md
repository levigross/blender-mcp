# Background MCP tasks

For a long Scheme evaluation, pass `background: true` to `scheme_eval`. The client
must declare `io.modelcontextprotocol/tasks` in its per-request capabilities.
The response contains `resultType: "task"` and a task ID instead of the evaluation
result. Ordinary calls remain synchronous, including calls from clients that do
not implement tasks. A background request without the capability is rejected
before evaluating its code.

With `rmcp`, use `ClientServiceExt::serve_with_lifecycle` in `Discover` mode so
the SDK sends capabilities with each request. A legacy initialization handshake
alone does not provide per-request capability negotiation on this stateless endpoint.

For the server's MCP 2026-07-28 protocol, a request looks like:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "scheme_eval",
    "arguments": {
      "code": "(scene-summary)",
      "background": true,
      "timeout_secs": 120
    },
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {
        "extensions": {"io.modelcontextprotocol/tasks": {}}
      }
    }
  }
}
```

Send it to `/mcp` with `Content-Type: application/json`,
`Accept: application/json, text/event-stream`, `MCP-Protocol-Version: 2026-07-28`,
`Mcp-Method: tools/call`, and `Mcp-Name: scheme_eval`. Poll `tasks/get` with
`{"taskId": "<returned ID>"}` and the same per-request metadata; use
`Mcp-Method: tasks/get` and `Mcp-Name: <returned ID>`. The task ID is also required
in `Mcp-Name` for `tasks/update` and `tasks/cancel`. Honor the returned `pollIntervalMs`
(currently 1,000 milliseconds).

In raw JSON-RPC responses, the task fields are flattened: the ID is
`result.taskId` and its state is `result.status`. The completed tool result is
`result.result`; there is no nested `task` object.

`tasks/get` includes the final `result` when `status` is `completed`, or the
protocol `error` when `status` is `failed`. The result is the same tool result
as a synchronous call, including structured output and artifacts. A tool-level
`isError: true` still has task status `completed`; inspect the tool result.
This follows the current [MCP Tasks extension](https://modelcontextprotocol.io/extensions/tasks/overview),
which uses `tasks/get`, `tasks/update`, and `tasks/cancel`. The older
`tasks/result` and `tasks/list` protocol is not exposed.

## Cancellation and lifetime

- `tasks/cancel` acknowledges cancellation immediately. Poll for the eventual
  outcome. Queued work and interruptible Scheme execution can become `cancelled`.
  A running Blender operator may continue; its tool error preserves
  `potentially_continuing` and request receipts so you can inspect before retrying.
- Tasks survive HTTP disconnects and can be polled by a reconnected authorized
  client. They share the same single-user Scheme environment and FIFO execution
  queue as synchronous calls. They do not run Blender operations concurrently.
- At most 32 task records, including retained results, are admitted. The task
  TTL is the evaluation timeout plus five minutes; completion is retained for
  one further TTL window by the SDK. Expired records are swept on task requests.
  The server rejects new tasks when retained capacity is exhausted.
- State is held in memory and ends on server restart. Saving a `.blend` does not
  save tasks or Scheme definitions. Resource and task access use the same HTTP
  authorization as tool calls; this is a shared single-user service.
- `tasks/update` accepts input responses as required by the extension. Scheme
  evaluations currently do not request client input.

`render-start`, `job-status`, `job-result`, and `job-cancel` remain available for
Blender render jobs. A task around `(render-start)` completes when the render job
is admitted; poll that job separately. A task around `(render!)` covers the
evaluation waiting for that render to finish.
