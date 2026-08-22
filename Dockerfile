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
ARG RUST_IMAGE=rust:1.97.1-alpine3.24@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900

# The builder always runs natively on the build platform and cross-compiles to
# the target, so a multi-platform image needs no emulation.
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS build

ARG TARGETARCH

WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# elfpak has no C dependencies, so rust-lld links every supported target and no
# cross toolchain is needed. `+crt-static` keeps the result a single file with
# no runtime dependencies of its own.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=elfpak-target-${TARGETARCH},sharing=locked <<'SHELL'
set -eu
case "${TARGETARCH}" in
    amd64) target=x86_64-unknown-linux-musl ;;
    arm64) target=aarch64-unknown-linux-musl ;;
    *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;;
esac
rustup target add "$target"
RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static" \
    cargo build --release --locked --bin elfpak --target "$target"
cp "target/$target/release/elfpak" /elfpak
SHELL

FROM scratch

COPY --from=build /elfpak /elfpak

ENTRYPOINT ["/elfpak"]
