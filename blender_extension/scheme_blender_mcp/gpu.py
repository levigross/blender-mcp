# SPDX-License-Identifier: GPL-3.0-or-later
"""Point Cycles at a GPU.

A Blender built with CUDA still renders on the CPU until something enables a device in
the Cycles addon preferences. Those preferences live in the user profile, and both the
throwaway profile used by ``scripts/live.sh`` and ``--factory-startup`` start empty, so
without this every session silently falls back to the CPU.
"""

from __future__ import annotations

from typing import Any

import bpy

# Best first. OptiX beats CUDA on any card that offers it; the rest are here so this
# stays useful on non-NVIDIA hardware.
BACKENDS = ("OPTIX", "CUDA", "HIP", "METAL", "ONEAPI")


def _refresh(preferences: Any) -> None:
    """Re-enumerate devices under whichever name this Blender publishes."""
    for name in ("refresh_devices", "get_devices"):
        method = getattr(preferences, name, None)
        if method is not None:
            method()
            return


def configure(backends: tuple[str, ...] = BACKENDS) -> dict[str, Any]:
    """Enable the best available GPU backend and switch every scene to it.

    Returns what was selected. A CPU-only build reports ``device_type: None`` rather
    than raising -- rendering still works, just slowly.
    """
    addon = bpy.context.preferences.addons.get("cycles")
    if addon is None:
        return {"device_type": None, "devices": [], "reason": "the cycles addon is not enabled"}
    preferences = addon.preferences

    for backend in backends:
        try:
            preferences.compute_device_type = backend
        except TypeError:
            continue  # this build was compiled without that backend
        _refresh(preferences)
        enabled = []
        for device in preferences.devices:
            device.use = device.type == backend
            if device.use:
                enabled.append(device.name)
        if enabled:
            for scene in bpy.data.scenes:
                scene.cycles.device = "GPU"
            return {"device_type": backend, "devices": enabled}

    return {
        "device_type": None,
        "devices": [],
        "reason": "no GPU device; this Blender was probably built without GPU compute",
    }
