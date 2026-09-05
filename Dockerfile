# syntax=docker/dockerfile:1

# Distribution image for the elfpak utility itself: a single static binary that
# can be copied into any builder image.
#
#   FROM ghcr.io/asaaki/elfpak:0.1 AS elfpak
#   COPY --from=elfpak /elfpak /usr/local/bin/elfpak
#
# Build all supported architectures at once:
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t elfpak:local --load .

# Base images are pinned by tag *and* digest so a build is reproducible and the
# toolchain never moves under us. Refresh a digest with:
#   docker buildx imagetools inspect <tag> --format '{{.Manifest.Digest}}'
ARG RUST_IMAGE=rust:1.98.0-alpine3.24@sha256:a10e64dd139b7387337c7fbe8aca31b959b57b2fd4c8ae20a02cf1d6ea424dce

# The builder always runs natively on the build platform and cross-compiles to
# the target, so a multi-platform image needs no emulation.
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS build

ARG TARGETARCH

WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# The two command-line tools have no C dependencies, so rust-lld links every
# supported target and no cross toolchain is needed. `+crt-static` keeps each
# result a single file with no runtime dependencies of its own.
#
# Both caches are keyed per architecture so the amd64 and arm64 builds do not
# clobber each other.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry-${TARGETARCH},sharing=locked \
    --mount=type=cache,target=/src/target,id=elfpak-target-${TARGETARCH},sharing=locked <<'SHELL'
set -eu
case "${TARGETARCH}" in
    amd64) target=x86_64-unknown-linux-musl ;;
    arm64) target=aarch64-unknown-linux-musl ;;
    *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;;
esac
rustup target add "$target"
RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static" \
    cargo build --release --locked --bin elfpak --bin cargo-elfpak --target "$target"
cp "target/$target/release/elfpak" /elfpak
cp "target/$target/release/cargo-elfpak" /cargo-elfpak
SHELL

# The release workflow exports this stage as a directory and publishes both
# binaries as target-specific GitHub release assets. Keep it separate from the
# final distribution image, whose contract remains one standalone elfpak tool.
FROM scratch AS release-binaries

COPY --from=build /elfpak /elfpak
COPY --from=build /cargo-elfpak /cargo-elfpak

FROM scratch

COPY --from=build /elfpak /elfpak

ENTRYPOINT ["/elfpak"]
