#!/bin/sh
# Copyright © 2026 Jalapeno Labs
#
# Closes the route around the Arsox egress proxy, then starts the satellite.
#
# The satellite cannot do this for itself. Removing a route inside a container
# needs NET_ADMIN, and a process that held NET_ADMIN for its whole life would be
# a larger thing than the gate it was raising. So the route closure is deployment
# configuration, and this script is what that configuration looks like.
#
# The rule is one sentence: the agent account may reach loopback and nothing
# else. Loopback is where the Arsox egress proxy listens, where the LLM proxy
# listens, and where a declared service listens, so an agent keeps everything it
# is meant to have and loses the route that would take it past the allowlist. The
# satellite runs as root and is untouched, which is what leaves its own outbound,
# the model requests and the `gh` polling, working exactly as it did.
#
# Usage, from a Dockerfile that extends the published image:
#
#     FROM jalapenolabs/arsox-satellite:ubuntu-1.0.0
#     USER root
#     RUN apt-get update \
#         && apt-get install --no-install-recommends --yes iptables \
#         && rm -rf /var/lib/apt/lists/*
#     COPY --chmod=0755 close-the-route.sh /usr/local/bin/close-the-route.sh
#     ENTRYPOINT [ "/usr/local/bin/close-the-route.sh" ]
#
# and then:
#
#     docker run --cap-add NET_ADMIN -p 8080:8080 ... your-image
#
# NET_ADMIN is held by root inside the container. Agents are not root and cannot
# use it, and it is scoped to the container's own network namespace as long as
# the container does not run with `--network host`.
#
# See docs/enforcement.md for what this closes, what it does not, and the
# alternative for a deployment that will not grant the capability.

set -eu

# The account the image creates for agents, matching the `useradd` in the
# Dockerfile and `privilege::AGENT_ACCOUNT`. Overridable for an image that
# renamed it, which is the same fact in a third place and worth being able to
# correct without editing this file.
AGENT_UID="${ARSOX_AGENT_UID:-10001}"

# Whether a route that could not be closed should stop the satellite from
# starting. It should: a satellite that came up believing it was enforcing, on a
# host where it is not, is worse off than one that refused to start. Set it to
# `false` deliberately, and only for a deployment that has accepted the advisory
# layer alone.
REQUIRE_CLOSURE="${ARSOX_REQUIRE_ROUTE_CLOSURE:-true}"

# Applies the rule to one address family.
#
# The loopback rule is appended first and matched first, so an agent reaching the
# proxies is accepted before the rule below it is ever consulted.
#
# The reject type is per family: `icmp-port-unreachable` does not exist for IPv6
# and ip6tables refuses the whole rule when it is named. Getting this wrong is
# how an IPv6 route stays open behind an IPv4 one that closed cleanly.
close_route() {
    binary="$1"
    reject="$2"

    command -v "$binary" >/dev/null 2>&1 || return 1

    "$binary" -A OUTPUT -o lo -j ACCEPT || return 1
    "$binary" -A OUTPUT -m owner --uid-owner "$AGENT_UID" \
        -j REJECT --reject-with "$reject" || return 1

    return 0
}

# Whether this network namespace has an IPv6 stack at all.
#
# The file is absent on a kernel built without IPv6, which is the one case where
# failing to filter IPv6 leaves no route open. Its emptiness says nothing: procfs
# reports every file as zero bytes.
has_ipv6() {
    [ -e /proc/net/if_inet6 ]
}

# Stops the satellite rather than starting it believing it is enforcing.
refuse() {
    echo "arsox: $1" >&2
    echo "arsox: install iptables and run the container with --cap-add NET_ADMIN" >&2

    if [ "$REQUIRE_CLOSURE" = "true" ]; then
        exit 1
    fi
}

if close_route iptables icmp-port-unreachable; then
    echo "arsox: uid ${AGENT_UID} may reach loopback only (ipv4)"
else
    refuse "could not close the ipv4 route for uid ${AGENT_UID}"
fi

# IPv6 is a route out wherever it exists, so it is closed exactly as strictly.
# Where the kernel has no IPv6 at all there is nothing to close and nothing to
# refuse over.
if ! has_ipv6; then
    echo "arsox: this kernel has no ipv6, so there is no ipv6 route to close"
elif close_route ip6tables icmp6-port-unreachable; then
    echo "arsox: uid ${AGENT_UID} may reach loopback only (ipv6)"
else
    refuse "could not close the ipv6 route for uid ${AGENT_UID}"
fi

exec /usr/local/bin/arsox-satellite "$@"
