# ce-net/headless-chrome — the render cell image for ce-render screenshot/PDF/scrape jobs.
#
# The cell is launched by a host via CE `mesh_deploy` with the argv ce-render builds:
#   ce-render-chrome --input /work/input --out /work/out --from <start> --to <to> [extra...]
# `/work/input` is the staged batch (a newline-delimited list of URLs, by CID); the wrapper renders
# each index in [from,to] and writes its capture (PNG or PDF) into /work/out, which the host captures
# as the cell's output blob and returns by CID.
#
# Determinism note: pin the Chromium version and disable timestamps/animations for the verify dial to
# be meaningful (a re-render on a second host should match). Build:
#   docker build -f images/chrome.Dockerfile .

FROM node:20-bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        chromium ca-certificates fonts-liberation libnss3 libatk-bridge2.0-0 \
        libgtk-3-0 libgbm1 libasound2 && \
    rm -rf /var/lib/apt/lists/*

ENV PUPPETEER_SKIP_CHROMIUM_DOWNLOAD=1 \
    PUPPETEER_EXECUTABLE_PATH=/usr/bin/chromium

# The thin render wrapper (`ce-render-chrome`) drives headless Chromium over the input batch. It is
# shipped alongside this image; copy it in here. It MUST accept: --input --out --from --to and any
# extra args ce-render appends (e.g. --pdf, --window-size=WxH).
# TODO(image): vendor the ce-render-chrome wrapper script (puppeteer-core based) into this image and
# COPY it here, then `ln -s` it onto PATH. The Rust side (proto::render_argv) already targets this
# exact CLI contract; the wrapper is the remaining runtime artifact.
WORKDIR /work
RUN mkdir -p /work/out
