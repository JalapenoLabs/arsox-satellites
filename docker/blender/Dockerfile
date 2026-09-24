# check=skip=InvalidDefaultArgInFrom
# Copyright © 2026 Jalapeno Labs

# The Blender satellite image: the Ubuntu satellite image, plus Blender and its
# MCP integration, so an agent can model whenever a task calls for it.
#
# It extends the base image rather than living inside it. Blender adds about two
# gigabytes, and most satellites never open a .blend file. See docs/blender.md.
#
# Built from the repository root, on top of a base image built first:
#
#   docker build --tag arsox-satellite:<tag> .
#   docker build --file docker/blender/Dockerfile \
#     --build-arg BASE_IMAGE=arsox-satellite:<tag> --tag arsox-satellite:<tag>-blender .

# No default: a floating base would let this image and the satellite it wraps drift.
# The check directive on the first line silences BuildKit's warning about exactly that.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

# Every download is pinned to an exact version and verified against a checksum
# or, for the MCP server, an exact commit.
ARG BLENDER_VERSION=5.2.2
ARG BLENDER_SHA256=84098912789dc450e95697c4184fb8a90acbe5111c2ba4aede3fecb57806a168
ARG BLENDER_MCP_COMMIT=2cea8d566dde07fbac28a61d698909d69724e853
ARG BLENDER_MCP_EXTENSION_VERSION=1.0.3
ARG BLENDER_MCP_EXTENSION_SHA256=a7a9da816192502e5a0a202a396444e266b47d8fc4f74ad4698048bd43040707

COPY docker/blender/install.sh docker/blender/configure_preferences.py docker/blender/requirements.txt \
    /tmp/blender-setup/
RUN sh /tmp/blender-setup/install.sh && rm -rf /tmp/blender-setup

COPY --chmod=0755 docker/blender/entrypoint.sh /usr/local/bin/arsox-blender-entrypoint

ENTRYPOINT [ "/usr/local/bin/arsox-blender-entrypoint" ]
