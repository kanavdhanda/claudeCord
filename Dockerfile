# syntax=docker/dockerfile:1.7
# The hosted hub as a container, in two stages: `build` has the Rust toolchain and is thrown away; the image that runs holds only the program,
# the CA certificates it needs to talk to Discord, and a data folder, and it runs as an unprivileged user.
#
# Run it with deploy/compose.yml. The Discord sign-in secret is a file mounted into the container, never an environment variable.
# Everything the hub keeps (databases, the `kek` key file, history) is under /data: back that volume up, above all `kek`.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Only what the program is built from: the dashboard's built files are embedded in it, so no Node is needed here.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
COPY web/dist ./web/dist
# The cache mounts keep downloaded crates and compiled dependencies between builds on the same machine.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin claudecord \
    && cp target/release/claudecord /claudecord \
    && mkdir /data

# glibc and the C runtime library, CA certificates, no shell and no package manager.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /claudecord /claudecord
# 65532 is the `nonroot` user. A named volume mounted here starts out owned by it.
COPY --from=build --chown=65532:65532 /data /data
WORKDIR /data
EXPOSE 8787
VOLUME /data
# The program asks the hub itself whether it is ready (there is no curl in this image). The prober keeps its small record in /tmp.
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD ["/claudecord", "probe", "http://127.0.0.1:8787", "--once", "--data", "/tmp/probe"]
ENTRYPOINT ["/claudecord"]
# Arguments come from deploy/compose.yml (`serve` needs the public address and the Discord application). Without them it prints its help.
CMD ["--help"]
