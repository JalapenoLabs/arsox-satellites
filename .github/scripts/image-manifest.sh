#!/usr/bin/env bash
# Copyright © 2026 Jalapeno Labs

# Reports every pinned package and tool version inside a satellite image.
#
# The README promises that "the exact set is published in the image's manifest",
# and this is what produces it. The release workflow runs it inside the image it
# just built, with the entrypoint overridden, and attaches the output to the
# GitHub release.
#
# It reads the image from the inside rather than from the Dockerfile on purpose.
# A Dockerfile says what was asked for; the running image says what is actually
# there, which is the only version a consumer can act on. Transitive packages
# that no ARG names show up here and nowhere else.
#
# This script describes what is in the image, never what the image is called.
# The workflow knows the tag and writes the header; the two facts come from the
# two places that actually hold them.

set -euo pipefail

echo '## Distribution'
# shellcheck disable=SC1091
. /etc/os-release
echo "${PRETTY_NAME}"
echo

# One line per tool, because several of these print a paragraph when asked.
echo '## Tools'
printf '%-8s %s\n' git "$(git --version)"
printf '%-8s %s\n' node "$(node --version)"
printf '%-8s %s\n' npm "$(npm --version)"
printf '%-8s %s\n' claude "$(claude --version | head -n 1)"
printf '%-8s %s\n' gh "$(gh --version | head -n 1)"
printf '%-8s %s\n' jira "$(jira version | head -n 1)"
printf '%-8s %s\n' python3 "$(python3 --version)"
printf '%-8s %s\n' pip "$(pip3 --version)"
echo

# Sorted so two manifests diff cleanly. dpkg's own order is installation order,
# which changes whenever a layer is rebuilt and makes every diff noise.
echo '## Distribution packages'
dpkg-query --show --showformat='${binary:Package}\t${Version}\n' | sort
