# SPDX-License-Identifier: GPL-3.0-or-later
"""Blender UI for the local Scheme MCP bridge."""

from __future__ import annotations

import bpy
from bpy.props import BoolProperty, IntProperty

from .bridge import get_server, start_live, stop_live


class SCHEMEMCP_Preferences(bpy.types.AddonPreferences):
    bl_idname = __package__

    auto_start: BoolProperty(
        name="Auto-start bridge",
        description="Start the loopback bridge after Blender loads a file",
        default=False,
    )
    port: IntProperty(
        name="Port",
        description="Loopback TCP port used by blender-mcp",
        default=9876,
        min=1024,
        max=65535,
    )

    def draw(self, _context: bpy.types.Context) -> None:
        layout = self.layout
        layout.prop(self, "auto_start")
        layout.prop(self, "port")


class SCHEMEMCP_OT_Start(bpy.types.Operator):
    bl_idname = "scheme_mcp.start_bridge"
    bl_label = "Start Scheme MCP Bridge"
    bl_options = {"INTERNAL"}

    def execute(self, context: bpy.types.Context) -> set[str]:
        preferences = context.preferences.addons[__package__].preferences
        try:
            start_live(preferences.port)
        except Exception as error:
            self.report({"ERROR"}, str(error))
            return {"CANCELLED"}
        return {"FINISHED"}


class SCHEMEMCP_OT_Stop(bpy.types.Operator):
    bl_idname = "scheme_mcp.stop_bridge"
    bl_label = "Stop Scheme MCP Bridge"
    bl_options = {"INTERNAL"}

    def execute(self, _context: bpy.types.Context) -> set[str]:
        stop_live()
        return {"FINISHED"}


class SCHEMEMCP_PT_Panel(bpy.types.Panel):
    bl_label = "Scheme MCP"
    bl_idname = "SCHEMEMCP_PT_panel"
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"
    bl_category = "Scheme MCP"

    def draw(self, context: bpy.types.Context) -> None:
        layout = self.layout
        preferences = context.preferences.addons[__package__].preferences
        server = get_server()
        if server is None:
            layout.operator(SCHEMEMCP_OT_Start.bl_idname, icon="PLAY")
            layout.label(text=f"Stopped · 127.0.0.1:{preferences.port}")
        else:
            layout.operator(SCHEMEMCP_OT_Stop.bl_idname, icon="PAUSE")
            layout.label(text=f"Listening · {server.address[0]}:{server.address[1]}")
            layout.label(text=f"Connections: {server.connection_count}")
            layout.label(text=f"Queued: {server.work.qsize()}")
            layout.label(text=f"Catalog: {server.operations.catalog_revision[:12]}")
            layout.label(text=f"Last request: {server.last_request or 'none'}")
            if server.last_error:
                box = layout.box()
                box.label(text="Last error", icon="ERROR")
                box.label(text=server.last_error[:160])
        layout.prop(preferences, "auto_start")


@bpy.app.handlers.persistent
def _auto_start(_unused: object) -> None:
    addon = bpy.context.preferences.addons.get(__package__)
    if addon is not None and addon.preferences.auto_start and get_server() is None:
        start_live(addon.preferences.port)


CLASSES = (
    SCHEMEMCP_Preferences,
    SCHEMEMCP_OT_Start,
    SCHEMEMCP_OT_Stop,
    SCHEMEMCP_PT_Panel,
)


def register() -> None:
    for cls in CLASSES:
        bpy.utils.register_class(cls)
    if _auto_start not in bpy.app.handlers.load_post:
        bpy.app.handlers.load_post.append(_auto_start)
    _auto_start(None)


def unregister() -> None:
    stop_live()
    if _auto_start in bpy.app.handlers.load_post:
        bpy.app.handlers.load_post.remove(_auto_start)
    for cls in reversed(CLASSES):
        bpy.utils.unregister_class(cls)
