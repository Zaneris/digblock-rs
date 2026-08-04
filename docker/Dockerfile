# Headless DigBlock server + bot, for local multiplayer testing (compose
# + tc netem network profiles). cargo-chef caches the dependency layer so
# code-only changes rebuild in seconds.
# Suite-pinned like docker/Dockerfile.release — build and runtime stages
# must ride the same Debian suite (glibc symbol versioning); bump both
# deliberately, never via `1-slim` drift.
FROM rust:1-slim-trixie AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json .
RUN cargo chef cook --release --recipe-path recipe.json -p digblock-server
COPY . .
RUN cargo build --release -p digblock-server --bin digblock-server --bin bot

FROM debian:trixie-slim
# iproute2 provides `tc` for the netem impairment profiles.
RUN apt-get update \
	&& apt-get install -y --no-install-recommends iproute2 ca-certificates \
	&& rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/digblock-server /app/target/release/bot /usr/local/bin/
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
EXPOSE 4433/udp
ENTRYPOINT ["/entrypoint.sh"]
CMD ["digblock-server", "--port", "4433", "--data-dir", "/data"]
