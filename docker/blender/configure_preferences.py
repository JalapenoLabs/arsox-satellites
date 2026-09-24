# Copyright © 2026 Jalapeno Labs
"""Saves the agent account's Blender preferences so the MCP bridge is ready on every launch.

Run inside Blender by install.sh, as the agent account, after the extension is installed.
Preferences live in the profile's userpref.blend and are only written when saved, so a
setting changed here and not saved would be gone by the next launch.
"""

from __future__ import annotations

import sys

import bpy

EXTENSION_MODULE = "bl_ext.user_default.mcp"

addon = bpy.context.preferences.addons.get(EXTENSION_MODULE)
if addon is None:
    print(f"[configure_preferences] {EXTENSION_MODULE} is not enabled, so the bridge would never start")
    sys.exit(1)

# The extension declares the network permission and refuses to listen without online access.
bpy.context.preferences.system.use_online_access = True

# Auto start covers a Blender opened with a window. Background mode ignores it and is started
# with `--command blender_mcp` by entrypoint.sh, so this only matters to an interactive session.
addon.preferences.use_autostart = True

bpy.ops.wm.save_userpref()
print(f"[configure_preferences] saved preferences with {EXTENSION_MODULE} enabled and online access on")
