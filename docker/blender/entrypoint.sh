#!/bin/sh
# Copyright © 2026 Jalapeno Labs
#
# Starts Blender's MCP services beside the satellite, then becomes the satellite.
#
# Two long-lived processes, both loopback only and both as the agent account:
#
#   the bridge   Blender in background mode with the `mcp` extension listening on
#                BLENDER_BRIDGE_PORT. One session, shared by every thread.
#   the server   Blender Lab's MCP server over streamable HTTP on BLENDER_MCP_PORT.
#                This is the address a host application declares as a thread's MCP
#                server. It forwards live-session tools to the bridge and runs the
#                `*_for_cli` tools as one `blender -b` per call.
#
# They run as the agent account because the bridge executes whatever Python an
# agent sends it. As root that would be a way out of every enforcement point; as
# the agent it is exactly the reach the agent's own shell already has. For the
# same reason they start from an empty environment: this script holds ARSOX_SECRET
# and any provider credential the container was given, and an agent that can run
# Python in Blender can read Blender's environment.
#
# The satellite stays PID 1 and root, via exec, so its enforcement posture and the
# image checks that assert it are unchanged. Each service is restarted if it exits,
# because a crashed Blender would otherwise leave every later thread without tools.

set -eu

agent_account=arsox
agent_home=/home/arsox
restart_delay_seconds=2

# These two numbers are a contract: the bridge port is what the server dials, and
# the server port is what host applications declare. Change them together, and
# change every host that declares the server.
BLENDER_BRIDGE_PORT=9876
BLENDER_MCP_PORT=9877

run_as_agent() {
    setpriv --reuid="$agent_account" --regid="$agent_account" --init-groups \
        env -i HOME="$agent_home" PATH=/usr/local/bin:/usr/bin:/bin LANG=C.UTF-8 "$@"
}

supervise() {
    service_name=$1
    shift
    while true; do
        "$@" || true
        echo "blender: $service_name exited, restarting in ${restart_delay_seconds}s" >&2
        sleep "$restart_delay_seconds"
    done
}

cd /workspace

supervise bridge run_as_agent blender --background --online-mode \
    --command blender_mcp --host 127.0.0.1 --port "$BLENDER_BRIDGE_PORT" &

supervise server run_as_agent \
    BLENDER_PATH=/opt/blender/blender \
    BLENDER_MCP_HOST=127.0.0.1 \
    BLENDER_MCP_PORT="$BLENDER_BRIDGE_PORT" \
    /opt/blender-mcp/bin/blender-mcp --transport http --host 127.0.0.1 --port "$BLENDER_MCP_PORT" &

exec /usr/local/bin/arsox-satellite "$@"
