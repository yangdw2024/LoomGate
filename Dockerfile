# =============================================================================
# LoomGate v1.0.0 — Multi-Stage Production Dockerfile
# Stage 1: Build unified React/Vite Web Console (apps/desktop/dist)
# Stage 2: Build Rust release binaries (gateway-daemon + cloud-control-plane)
# Stage 3: Minimal Debian Slim runtime serving 3-protocol Gateway + Web Console
# =============================================================================

# --- Stage 1: Frontend Web Console ---
FROM node:22-alpine AS web-builder
WORKDIR /src/apps/desktop
COPY apps/desktop/package.json apps/desktop/package-lock.json ./
RUN npm ci
COPY apps/desktop/ ./
RUN npm run build

# --- Stage 2: Rust Release Binaries ---
FROM rust:bookworm AS rust-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Strip Tauri desktop crate from workspace build inside headless Linux container
RUN sed -i '/apps\/desktop\/src-tauri/d' Cargo.toml \
 && cargo build --release -p gateway-daemon -p cloud-control-plane

# --- Stage 3: Minimal Runtime Image ---
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
RUN mkdir -p /data

COPY --from=rust-builder /src/target/release/gateway-daemon /usr/local/bin/gateway-daemon
COPY --from=rust-builder /src/target/release/cloud-control-plane /usr/local/bin/cloud-control-plane
COPY --from=web-builder /src/apps/desktop/dist /app/web-dist
COPY config.example.toml /app/config.example.toml

ENV GATEWAY_BIND_ADDR=0.0.0.0:8787 \
    GATEWAY_CONFIG=/app/config.example.toml \
    GATEWAY_PERSIST_CONFIG=/data/config.local.toml \
    GATEWAY_DB_PATH=/data/trajectory.db \
    LOOMGATE_WEB_DIST=/app/web-dist \
    RUST_LOG=gateway_daemon=info,gateway_core=info

EXPOSE 8787
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8787/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/gateway-daemon"]
