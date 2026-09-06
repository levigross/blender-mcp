# SPDX-License-Identifier: GPL-3.0-or-later
"""Main-thread Blender operation dispatcher.

The dispatcher and its RNA allowlist live in the native module, where they are covered
by the workspace's Rust tests rather than being the one untested part of the sandbox.
This module remains as the import path `bridge.py` already uses.
"""

from __future__ import annotations

from .scheme_blender_mcp_native import BlenderOperations, OperationError

__all__ = ["BlenderOperations", "OperationError"]
