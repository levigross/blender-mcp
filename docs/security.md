# Security and trust

The Steel environment blocks direct filesystem, process, dynamic-library,
network, module-load, and dynamic-evaluation capabilities. CPU-bound bytecode
is interrupted by a watchdog; values and protocol frames have explicit limits.

This does not make Blender untrusted-code-safe. Blender operators can open,
save, import, export, and render files, and enabled extensions can register
arbitrary operators. Treat complete Blender access as trusted local authority.

Loopback is the default. Non-loopback requires both explicit opt-in and bearer
authentication. v1 has no TLS; use an external TLS terminator on any untrusted
network.

Queued Blender work is cancellable. A running main-thread operator may continue
after the caller times out, which is reported explicitly.

