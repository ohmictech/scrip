# Build stage. Pinned to the same toolchain family as CI so the binary matches
# the released archives closely enough for the same behaviour.
FROM rust:1-slim-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# `gpu` adds the OpenCL backend, which is loaded at run time: the image still
# works without a GPU, and a GPU container needs `--device /dev/dri`.
RUN cargo build --release --locked --features gpu

# Runtime stage. The binary links glibc, and TLS roots are compiled in via
# webpki-roots, so there is nothing to install alongside it.
FROM debian:bookworm-slim

RUN useradd --create-home --uid 10001 scrip
COPY --from=build /src/target/release/scrip-miner /usr/local/bin/scrip-miner

# config.toml and wallet.json are created here, so mount a volume at /data or
# the wallet is lost when the container is replaced.
WORKDIR /data
RUN chown scrip:scrip /data
USER scrip
VOLUME ["/data"]

ENTRYPOINT ["scrip-miner"]
