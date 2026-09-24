#!/bin/sh
# Copyright © 2026 Jalapeno Labs
#
# Installs Blender and its MCP integration into the satellite image, so an agent
# can model, render, and inspect .blend files without installing anything itself.
#
# Three pieces, each pinned and verified the same way the base image pins Node and gh:
#
#   /opt/blender         Blender itself, from the official release tarball.
#   /opt/blender-mcp     The MCP server (Blender Lab's `blender-mcp`), in its own venv.
#                        It is what the harness speaks to over streamable HTTP.
#   the `mcp` extension  Blender Lab's bridge add-on, installed and enabled for the
#                        agent account. It is what lets the server run code inside a
#                        live Blender session rather than one `blender -b` per call.
#
# Run as root from docker/blender/Dockerfile, which supplies every version and
# checksum as a build argument. The runtime half is entrypoint.sh.

set -eu

: "${BLENDER_VERSION:?}" "${BLENDER_SHA256:?}"
: "${BLENDER_MCP_COMMIT:?}" "${BLENDER_MCP_EXTENSION_VERSION:?}" "${BLENDER_MCP_EXTENSION_SHA256:?}"

setup_directory=$(dirname "$0")
agent_account=arsox
agent_home=/home/arsox

# Blender's Linux build links the X11 and GL client libraries even in background
# mode, and loads them before it knows it will never open a window. Nothing here
# draws: these only satisfy the loader. Pinned exactly, like the base image.
apt-get update
apt-get install --no-install-recommends --yes \
    libx11-6=2:1.8.7-1build1 \
    libxi6=2:1.8.1-1build1 \
    libxxf86vm1=1:1.1.4-1build4 \
    libxfixes3=1:6.0.0-2build1 \
    libxrender1=1:0.9.10-1.1build1 \
    libxkbcommon0=1.6.0-1build1 \
    libsm6=2:1.2.3-1build3 \
    libice6=2:1.0.10-1build3 \
    libgl1=1.7.0-1build1 \
    libegl1=1.7.0-1build1
rm -rf /var/lib/apt/lists/*

# Blender, from the release tarball. The tarball carries its own Python, so the
# bpy scripts agents write never meet the system interpreter.
blender_series=$(echo "$BLENDER_VERSION" | cut -d. -f1,2)
blender_url="https://download.blender.org/release/Blender${blender_series}/blender-${BLENDER_VERSION}-linux-x64.tar.xz"
curl -fsSLo /tmp/blender.tar.xz "$blender_url"
echo "${BLENDER_SHA256}  /tmp/blender.tar.xz" | sha256sum --check --quiet
mkdir -p /opt/blender
tar -xJf /tmp/blender.tar.xz -C /opt/blender --strip-components=1 --no-same-owner
rm /tmp/blender.tar.xz
ln -s /opt/blender/blender /usr/local/bin/blender
blender --version

# The MCP server, from Blender Lab's repository at an exact commit. Its release
# archives sit behind a bot check that refuses scripted downloads, so the commit
# hash is the pin: git verifies the content it names. Its dependencies install
# from a hash-locked file first, and the server itself with `--no-deps`, so pip
# never resolves anything that file does not name.
git init --quiet /tmp/blender-mcp-source
git -C /tmp/blender-mcp-source fetch --quiet --depth 1 \
    https://projects.blender.org/lab/blender_mcp.git "$BLENDER_MCP_COMMIT"
git -C /tmp/blender-mcp-source checkout --quiet FETCH_HEAD
python3 -m venv /opt/blender-mcp
/opt/blender-mcp/bin/pip install --quiet --no-cache-dir --require-hashes \
    --requirement "$setup_directory/requirements.txt"
/opt/blender-mcp/bin/pip install --quiet --no-cache-dir --no-deps --no-build-isolation \
    /tmp/blender-mcp-source/mcp
rm -rf /tmp/blender-mcp-source

# The bridge extension, installed into the agent account's own Blender profile,
# because that is the account Blender runs as. Preferences are global to a
# profile and only persist when saved, so the script saves them explicitly:
# the extension enabled, online access allowed (the extension declares the
# network permission and will not listen without it), and auto start on.
extension_url="https://projects.blender.org/lab/blender_mcp/releases/download/v${BLENDER_MCP_EXTENSION_VERSION}"
extension_url="${extension_url}/mcp-${BLENDER_MCP_EXTENSION_VERSION}.zip"
curl -fsSLo /tmp/blender-mcp-extension.zip "$extension_url"
echo "${BLENDER_MCP_EXTENSION_SHA256}  /tmp/blender-mcp-extension.zip" | sha256sum --check --quiet
chmod 0644 /tmp/blender-mcp-extension.zip

run_as_agent() {
    setpriv --reuid="$agent_account" --regid="$agent_account" --init-groups \
        env -i HOME="$agent_home" PATH=/usr/local/bin:/usr/bin:/bin "$@"
}

run_as_agent blender --background --factory-startup --online-mode \
    --command extension install-file --repo user_default --enable /tmp/blender-mcp-extension.zip
run_as_agent blender --background --online-mode --python "$setup_directory/configure_preferences.py"
rm /tmp/blender-mcp-extension.zip
