FROM node:24.22.1-bookworm-slim@sha256:be8204d4bd327363357f532187b752ff62759c31bedf98e9e18fc08c9cbd605c AS node
FROM oven/bun:1.4.2-debian@sha256:4f6e31d1a54d6a3dd312daef655fc998101b5043d52e12592ac293ef04b9bc73 AS bun
FROM rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c
COPY --from=node /usr/local /usr/local
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git python3 musl-tools shellcheck xvfb xauth \
    libasound2 libatk-bridge2.0-0 libatk1.0-0 libcups2 libdbus-1-3 libdrm2 \
    libgbm1 libgtk-3-0 libnss3 libx11-xcb1 libxcomposite1 libxdamage1 \
    libxfixes3 libxkbcommon0 libxrandr2 libxss1 libxtst6 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy \
    && rustup target add wasm32-wasip2 x86_64-unknown-linux-musl
ENV CI=true CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 NPM_CONFIG_UPDATE_NOTIFIER=false
WORKDIR /workspace
COPY . .
CMD ["bash", ".buildkite/run.sh"]
