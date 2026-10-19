# syntax=docker/dockerfile:1
#
# The SonicCraft CLI and MCP server with ffmpeg (ogg, opus and m4a output), on Debian slim.
#
#   docker build -t soniccraft .                                   # from source
#   docker run --rm -v "$PWD":/work soniccraft render timeline.yaml
#   docker run -i --rm -v "$PWD":/work soniccraft mcp --base-dir /work
#
# The published image (ghcr.io/<owner>/soniccraft, .github/workflows/container.yml) is built with
# `--build-arg BINARIES=prebuilt` from a release's Linux binaries, copied into dist/linux-<arch>/, so it
# runs exactly what the release ships.

ARG BINARIES=source

FROM rust:1-trixie AS build-source
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p soniccraft-cli \
    && mkdir -p /out \
    && cp target/release/soniccraft target/release/soniccraft-mcp /out/

FROM scratch AS build-prebuilt
ARG TARGETARCH
COPY dist/linux-${TARGETARCH}/soniccraft dist/linux-${TARGETARCH}/soniccraft-mcp /out/

FROM build-${BINARIES} AS binaries

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY --from=binaries /out/soniccraft /out/soniccraft-mcp /usr/local/bin/
# Not root: files written to a mounted directory belong to uid 1000 (pass `--user "$(id -u):$(id -g)"`
# to write as yourself).
RUN useradd --create-home --uid 1000 soniccraft
USER soniccraft
WORKDIR /work
ENTRYPOINT ["soniccraft"]
CMD ["--help"]
