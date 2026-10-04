# The hub as a small static image. Not built on the development machine (no Docker there), so build it once and run
# `claudecord selftest` inside it before relying on it.
#
# Secrets (the Discord token, bucket keys) live in files under the data folder, never in environment variables. Mount a volume
# at /data, set them up once with `claudecord discord set --data /data ...` and `claudecord storage ... --data /data`, and put a
# TLS proxy in front of port 8787.
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN cargo build --release --bin claudecord

FROM scratch
COPY --from=build /src/target/release/claudecord /claudecord
EXPOSE 8787
VOLUME /data
# The image holds only the program, so the check is the program itself asking the hub if it is ready. Run with `--restart unless-stopped`
# so Docker also starts it again if it dies, and replaces it if this check keeps failing under an orchestrator that acts on it.
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 CMD ["/claudecord", "probe", "http://127.0.0.1:8787", "--once"]
ENTRYPOINT ["/claudecord"]
CMD ["hub", "--data", "/data", "--bind", "0.0.0.0:8787", "--allow-plain"]
