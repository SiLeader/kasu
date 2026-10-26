# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked -p kasu-server -p kasu-ingest && \
    install -D target/release/kasu-server /out/kasu-server && \
    install -D target/release/kasu-ingest /out/kasu-ingest

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tar && rm -rf /var/lib/apt/lists/* && \
    mkdir /data && chown 10001:10001 /data
COPY --from=build /out/kasu-server /out/kasu-ingest /usr/local/bin/
USER 10001:10001
EXPOSE 50051 50080
ENTRYPOINT ["kasu-server"]
