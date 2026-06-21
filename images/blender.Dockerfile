# ce-net/blender — the render cell image for ce-render Blender jobs.
#
# The cell is launched by a host via CE `mesh_deploy` with the argv ce-render builds:
#   blender -b /work/input -o /work/out/frame_ -s <start> -e <end> -a [extra...]
# The host stages the input asset (scene.blend, by CID) at /work/input before launch and captures
# /work/out as the cell's output blob (the rendered frames), returning its CID to the client.
#
# Pin a specific Blender version: determinism is load-bearing for the verify dial (a re-render on a
# second host must produce byte-identical frames). Build:  docker build -f images/blender.Dockerfile .

FROM debian:bookworm-slim

ARG BLENDER_VERSION=4.2.1
ARG BLENDER_MAJOR=4.2

# Headless Blender needs these even with -b (EGL/OpenGL libs for the render pipeline).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates xz-utils libx11-6 libxi6 libxxf86vm1 libxfixes3 libxrender1 \
        libgl1 libegl1 libgomp1 && \
    rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    url="https://download.blender.org/release/Blender${BLENDER_MAJOR}/blender-${BLENDER_VERSION}-linux-x64.tar.xz"; \
    curl -fsSL "$url" -o /tmp/blender.tar.xz; \
    mkdir -p /opt/blender; \
    tar -xJf /tmp/blender.tar.xz -C /opt/blender --strip-components=1; \
    rm /tmp/blender.tar.xz; \
    ln -s /opt/blender/blender /usr/local/bin/blender

WORKDIR /work
RUN mkdir -p /work/out

# argv is provided by the deploy; no ENTRYPOINT so ce-render's command runs verbatim.
