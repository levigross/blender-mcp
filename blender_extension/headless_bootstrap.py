# SPDX-License-Identifier: GPL-3.0-or-later
"""Start the shared bridge dispatcher inside ``blender --background``."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path


def _arguments() -> argparse.Namespace:
    arguments = sys.argv[sys.argv.index("--") + 1 :] if "--" in sys.argv else []
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    return parser.parse_args(arguments)


def main() -> None:
    extension_dir = Path(os.environ["BLENDER_MCP_EXTENSION_DIR"]).resolve()
    sys.path.insert(0, str(extension_dir.parent))
    from scheme_blender_mcp.bridge import run_headless

    # `--factory-startup` gives Cycles empty preferences, so without this a
    # GPU-capable build renders on the CPU and says nothing about it.
    if os.environ.get("BLENDER_MCP_GPU", "1") != "0":
        from scheme_blender_mcp.gpu import configure

        selection = configure()
        print(f"SCHEME_MCP: cycles device {selection}", file=sys.stderr)

    arguments = _arguments()
    run_headless(arguments.host, arguments.port)


if __name__ == "__main__":
    main()

