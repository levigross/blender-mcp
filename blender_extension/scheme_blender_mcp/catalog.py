# SPDX-License-Identifier: GPL-3.0-or-later
"""Runtime operator catalog derived from Blender RNA.

Implemented in the native module; kept here as a stable import path.
"""

from __future__ import annotations

from .scheme_blender_mcp_native import (
    build_catalog,
    describe_operator,
    idname_from_steel,
    steel_name,
)

__all__ = ["build_catalog", "describe_operator", "idname_from_steel", "steel_name"]
