# syntax=docker/dockerfile:1
#
# Production images, one build target per component:
#   docker build -f docker/Dockerfile --target server -t kasu/server .
#   docker build -f docker/Dockerfile --target web -t kasu/web .
# Only the stages a target depends on are built.

FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS source
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

FROM source AS build-server
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked -p kasu-server -p kasu-ingest --features kasu-server/production && \
    install -D -t /out target/release/kasu-server target/release/kasu-ingest

FROM source AS build-web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked -p kasu-web && \
    install -D -t /out target/release/kasu-web

FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.source="https://github.com/SiLeader/kasu" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && \
    mkdir /data && chown 10001:10001 /data
USER 10001:10001

# kasu-server listens on 50051 (gRPC) and 50080 (HTTP) and stores data in /data.
# Set KASU_TOKEN (or the other variables in docs/) to enable authentication.
FROM runtime AS server
LABEL org.opencontainers.image.title="kasu-server" \
      org.opencontainers.image.description="kasu server (gRPC, HTTP, Git gateway) with PostgreSQL and S3 backends"
COPY --from=build-server /out/kasu-server /out/kasu-ingest /usr/local/bin/
VOLUME /data
EXPOSE 50051 50080
ENTRYPOINT ["kasu-server"]
CMD ["/data", "0.0.0.0:50051", "0.0.0.0:50080"]

# kasu-web is configured with KASU_WEB_* environment variables (docs/web-ui.md);
# KASU_WEB_SERVER, KASU_WEB_SESSION_KEY_FILE and KASU_WEB_PUBLIC_URL are required.
FROM runtime AS web
LABEL org.opencontainers.image.title="kasu-web" \
      org.opencontainers.image.description="kasu Web UI"
COPY --from=build-web /out/kasu-web /usr/local/bin/
ENV KASU_WEB_LISTEN=0.0.0.0:50090
EXPOSE 50090
ENTRYPOINT ["kasu-web"]
