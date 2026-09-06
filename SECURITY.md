# Security model

`blender-mcp` is a single-user local automation service, not a multi-tenant
sandbox.

The HTTP server binds to `127.0.0.1:8000` by default and validates Host and
Origin. A non-loopback bind requires `--allow-non-loopback` and a bearer token
loaded from an environment variable or token file. TLS and OAuth are not part
of v1; use an external TLS terminator before sending a bearer token over an
untrusted network.

Steel's filesystem, process, dynamic-library, network, module-loading, and
evaluation escape paths are disabled or rejected. Resource limits bound
evaluation time, bridge frames, serialization depth/items, and response bytes.
Steel 0.8 does not provide a heap quota.

Blender access remains trusted. `bpy.ops` can read and write files, and enabled
third-party Blender extensions contribute operators to the runtime catalog.
Do not expose this service to untrusted callers.

A Blender operator that has begun on the main thread cannot always be safely
preempted. Timeout errors mark such work as potentially continuing.

Report vulnerabilities through
[GitHub's private vulnerability reporting](https://github.com/levigross/blender-mcp/security/advisories/new).
Do not include tokens, private scene data, or other secrets in public reports.
