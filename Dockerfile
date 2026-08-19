# syntax=docker/dockerfile:1

# Distribution image for the elfpak utility itself: a single static binary that
# can be copied into any builder image.
#
#   FROM ghcr.io/example/elfpak:0.1 AS elfpak
#   COPY --from=elfpak /elfpak /usr/local/bin/elfpak

# Base images are pinned by tag *and* digest so a build is reproducible and the
# toolchain never moves under us. Refresh a digest with:
#   docker buildx imagetools inspect <tag> --format '{{.Manifest.Digest}}'
ARG RUST_IMAGE=rust:1.97.1-alpine3.24@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900

FROM ${RUST_IMAGE} AS build

# Static linking against musl needs the C runtime objects. The package version
# is whatever alpine 3.24 currently ships; the base image digest pins the rest.
RUN apk add --no-cache musl-dev

WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin elfpak \
    && cp target/release/elfpak /elfpak

FROM scratch

COPY --from=build /elfpak /elfpak

ENTRYPOINT ["/elfpak"]
