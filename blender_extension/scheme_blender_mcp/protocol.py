# SPDX-License-Identifier: GPL-3.0-or-later
"""Wire framing shared with the Rust protocol crate.

The implementation moved into the native module so that the format has exactly one
definition, in `blender-mcp-protocol`, rather than a Rust encoder and a Python encoder
that can drift. They did drift: Python wrote ``"result": null`` for every Blender call
returning ``None``, which the server decoded as an absent field and rejected.

This module remains as the import path the rest of the extension already uses.
"""

from __future__ import annotations

from .scheme_blender_mcp_native import (
    MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
    ProtocolError,
    failure,
    read_frame,
    success,
    write_frame,
)

__all__ = [
    "MAX_FRAME_BYTES",
    "PROTOCOL_VERSION",
    "ProtocolError",
    "failure",
    "read_frame",
    "success",
    "write_frame",
]
