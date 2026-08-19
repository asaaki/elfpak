#!/usr/bin/env bash
# Docker smoke tests for elfpak.
#
#   tests/docker/smoke.sh              # everything
#   tests/docker/smoke.sh axum         # Axum on scratch, host architecture
#   tests/docker/smoke.sh axum-arm64   # Axum on scratch, linux/arm64
#   tests/docker/smoke.sh ca           # CA roots come from the bundle, not the binary
#   tests/docker/smoke.sh cross        # non-Rust cross-architecture packaging
#
# Requires docker. Everything involving linux/arm64 additionally requires qemu
# binfmt support and is skipped when that is unavailable. On an x86_64 host the
# arm64 tests run fully emulated, so `axum-arm64` compiles the whole dependency
# tree under qemu and takes a long time; `cross` stays cheap because it never
# compiles anything for the foreign architecture.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

# Pinned by tag and digest; refresh with:
#   docker buildx imagetools inspect <tag> --format '{{.Manifest.Digest}}'
debian_image="debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258"

cross_image="elfpak-cross:local"
port="${ELFPAK_SMOKE_PORT:-18080}"

# Platform docker itself runs on, e.g. linux/amd64.
host_platform="$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')"

# Resources the current test owns; cleaned up on exit or between tests. A RETURN
# trap cannot be used here: it stays installed after the function returns and
# would fire again in a scope where its locals no longer exist.
container_to_remove=""
workdir_to_remove=""

cleanup() {
    if [ -n "$container_to_remove" ]; then
        docker rm -f "$container_to_remove" >/dev/null 2>&1 || true
        container_to_remove=""
    fi
    if [ -n "$workdir_to_remove" ]; then
        # The exported sysroot and the generated rootfs are root-owned, so the
        # cleanup needs the same privileges that created them.
        docker run --rm -v "$workdir_to_remove:/work" "$debian_image" \
            rm -rf /work/sysroot /work/out >/dev/null 2>&1 || true
        rm -rf "$workdir_to_remove"
        workdir_to_remove=""
    fi
}
trap cleanup EXIT

log()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '  ok: %s\n' "$*"; }
skip() { printf '  skip: %s\n' "$*"; }

# Short platform label used in image tags: linux/arm64 -> arm64.
platform_tag() { printf '%s' "${1##*/}"; }

arm64_available() {
    docker run --rm --platform linux/arm64 "$debian_image" true >/dev/null 2>&1
}

# Build the elfpak distribution image for a platform. Progress goes to stderr so
# that the image name is the only thing on stdout.
build_elfpak() {
    local platform="$1"
    local image="elfpak:local-$(platform_tag "$platform")"
    {
        log "building $image for $platform"
        docker build --platform "$platform" -q -t "$image" -f Dockerfile . >/dev/null
        docker run --rm --platform "$platform" "$image" --version
    } >&2
    printf '%s' "$image"
}

# Build the Axum fixture, package it with elfpak, run the scratch image and
# assert everything the `web` preset promises.
#
#   $1 platform, $2 elfpak image, $3 published port, $4 readiness attempts
test_axum_on() {
    local platform="$1" elfpak_image="$2" http_port="$3" attempts="$4"
    local image="elfpak-axum:local-$(platform_tag "$platform")"

    log "building the Axum fixture for $platform and packaging it with elfpak"
    docker build --platform "$platform" -t "$image" \
        --build-arg "ELFPAK_IMAGE=$elfpak_image" \
        -f tests/docker/Dockerfile.axum .

    log "running the $platform scratch image"
    local id
    id="$(docker run -d --rm --platform "$platform" -p "$http_port:8080" "$image")"
    container_to_remove="$id"

    local base="http://127.0.0.1:$http_port"
    local ready=""
    for _ in $(seq 1 "$attempts"); do
        if curl -fsS "$base/health" >/dev/null 2>&1; then ready=1; break; fi
        sleep 0.2
    done
    [ -n "$ready" ] || { docker logs "$id"; fail "server did not start"; }
    ok "process started and bound to an unprivileged port"

    [ "$(curl -fsS "$base/health")" = "ok" ] || fail "/health"
    ok "/health"

    local arch expected
    arch="$(docker image inspect "$image" --format '{{.Architecture}}')"
    expected="$(platform_tag "$platform")"
    [ "$arch" = "$expected" ] || fail "expected a $expected image, got $arch"
    ok "image architecture is $arch"

    local who
    who="$(curl -fsS "$base/whoami")"
    grep -q '"uid":"65532"' <<<"$who" || fail "expected uid 65532, got $who"
    ok "runs as a non-root user ($who)"

    local tmp
    tmp="$(curl -fsS "$base/tmp")"
    grep -q '"ok":true' <<<"$tmp" || fail "/tmp is not writable: $tmp"
    ok "/tmp is writable"

    local dns
    dns="$(curl -fsS "$base/dns")"
    grep -q '"ok":true' <<<"$dns" || fail "DNS lookup failed: $dns"
    ok "DNS resolution works"

    local https
    https="$(curl -fsS "$base/outbound")"
    grep -q '"status":200' <<<"$https" || fail "outbound HTTPS failed: $https"
    ok "outbound HTTPS works with no application-side CA configuration"

    local pinned
    pinned="$(curl -fsS "$base/outbound/pinned")"
    grep -q '"status":200' <<<"$pinned" || fail "pinned HTTPS failed: $pinned"
    ok "the bundled CA file is also usable directly (opt-in)"

    # No shell, no package manager, nothing but the plan.
    docker run --rm --platform "$platform" --entrypoint /bin/sh "$image" -c true 2>/dev/null \
        && fail "the image contains a shell" || ok "no shell in the image"

    cleanup
}

test_axum() {
    local elfpak_image
    elfpak_image="$(build_elfpak "$host_platform")"
    test_axum_on "$host_platform" "$elfpak_image" "$port" 50
}

test_axum_arm64() {
    if [ "$host_platform" != "linux/arm64" ] && ! arm64_available; then
        skip "linux/arm64 emulation is unavailable"
        return 0
    fi
    local elfpak_image
    elfpak_image="$(build_elfpak linux/arm64)"
    # Emulated startup is an order of magnitude slower than native.
    test_axum_on linux/arm64 "$elfpak_image" "$((port + 1))" 300
}

# The same application, bundled with `minimal` instead of `web`, must fail to
# make an HTTPS request: that is what proves the CA roots come from the bundle
# and not from something baked into the binary.
test_ca_policy() {
    local elfpak_image image id base http_port
    elfpak_image="$(build_elfpak "$host_platform")"
    image="elfpak-axum:local-minimal"
    http_port="$((port + 2))"

    log "building the same service with --preset minimal"
    docker build --platform "$host_platform" -t "$image" \
        --build-arg "ELFPAK_IMAGE=$elfpak_image" \
        --build-arg "ELFPAK_PRESET=minimal" \
        -f tests/docker/Dockerfile.axum .

    id="$(docker run -d --rm --platform "$host_platform" -p "$http_port:8080" "$image")"
    container_to_remove="$id"
    base="http://127.0.0.1:$http_port"

    local ready=""
    for _ in $(seq 1 50); do
        if curl -fsS "$base/health" >/dev/null 2>&1; then ready=1; break; fi
        sleep 0.2
    done
    [ -n "$ready" ] || { docker logs "$id"; fail "minimal-preset server did not start"; }
    ok "the ELF closure alone is enough to start the service"

    local https
    https="$(curl -fsS "$base/outbound")"
    grep -q '"ok":false' <<<"$https" \
        || fail "HTTPS should not work without a CA bundle: $https"
    ok "outbound HTTPS fails without the CA bundle, as expected"

    cleanup
}

# Package a foreign-architecture binary from an exported sysroot. Nothing is
# compiled and nothing is executed for the target architecture.
test_cross() {
    if ! arm64_available; then
        skip "linux/arm64 emulation is unavailable"
        return 0
    fi
    local elfpak_image
    elfpak_image="$(build_elfpak "$host_platform")"

    log "exporting an aarch64 sysroot"
    local work
    work="$(mktemp -d)"
    workdir_to_remove="$work"
    mkdir -p "$work/sysroot" "$work/out"

    local container
    container="$(docker create --platform linux/arm64 "$debian_image")"
    docker export "$container" | tar -x -C "$work/sysroot" 2>/dev/null || true
    docker rm "$container" >/dev/null

    log "packaging an aarch64 binary with the $(platform_tag "$host_platform") elfpak"
    docker run --rm \
        -v "$work/sysroot:/sysroot:ro" \
        -v "$work/out:/out" \
        "$elfpak_image" bundle /sysroot/bin/ls \
            --root /sysroot \
            --output /out/rootfs \
            --install /app/ls \
            --preset minimal \
            -v

    grep -q '"architecture": "aarch64"' "$work/out/elfpak-manifest.json" \
        || fail "expected an aarch64 manifest"
    ok "resolved an aarch64 closure without executing anything"

    log "running the aarch64 scratch image"
    cp tests/docker/Dockerfile.cross "$work/out/Dockerfile"
    docker build --platform linux/arm64 -q -t "$cross_image" "$work/out" >/dev/null
    docker run --rm --platform linux/arm64 "$cross_image" -la / >/dev/null \
        || fail "the aarch64 image did not run"
    ok "aarch64 scratch image runs"

    cleanup
}

case "${1:-all}" in
    axum)       test_axum ;;
    axum-arm64) test_axum_arm64 ;;
    ca)         test_ca_policy ;;
    cross)      test_cross ;;
    all)        test_axum; test_ca_policy; test_axum_arm64; test_cross ;;
    *)          fail "unknown test: $1" ;;
esac

log "all smoke tests passed"
