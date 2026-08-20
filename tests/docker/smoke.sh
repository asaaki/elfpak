#!/usr/bin/env bash
# Docker smoke tests for elfpak.
#
#   tests/docker/smoke.sh              # everything
#   tests/docker/smoke.sh axum         # Axum on scratch, host architecture
#   tests/docker/smoke.sh axum-arm64   # Axum on scratch, linux/arm64
#   tests/docker/smoke.sh ca           # CA roots come from the bundle, not the binary
#   tests/docker/smoke.sh musl         # a dynamically linked musl program
#   tests/docker/smoke.sh ldcache      # a library the loader only finds through a cache
#   tests/docker/smoke.sh tar          # the same service delivered as a tar and ADDed
#   tests/docker/smoke.sh verify       # `elfpak verify` as a build gate
#   tests/docker/smoke.sh cross        # non-Rust cross-architecture packaging
#
#   tests/docker/smoke.sh --fresh [test]   # rebuild everything from nothing
#
# `--fresh` removes the images the suite owns and passes `--no-cache` to every
# build, so a rerun cannot be explained by a layer that was already there. It
# does not discard BuildKit cache mounts, which is what keeps cargo from
# recompiling the fixtures from scratch; clear those too with:
#
#   docker builder prune --filter type=exec.cachemount
#
# Requires docker. Anything involving linux/arm64 additionally requires qemu
# binfmt support and is skipped when that is unavailable.
#
# Images are built for every architecture under test in a single buildx
# invocation, which builds the variants in parallel and yields one tag that
# docker resolves per platform:
#
#   https://docs.docker.com/build/building/multi-platform/
#
# The elfpak image cross-compiles, so its arm64 variant costs no emulation at
# all. The Axum fixture is an ordinary application build and its arm64 variant
# does compile under qemu, which is why `axum-arm64` is the slow one.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

# Pinned by tag and digest; refresh with:
#   docker buildx imagetools inspect <tag> --format '{{.Manifest.Digest}}'
debian_image="debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258"

elfpak_image="elfpak:local"
axum_image="elfpak-axum:local"
musl_image="elfpak-musl:local"
ldcache_image="elfpak-ldcache:local"
tar_image="elfpak-tar:local"
verify_image="elfpak-verify:local"
cross_image="elfpak-cross:local"
port="${ELFPAK_SMOKE_PORT:-18080}"

# Appended to every build. A single token or nothing at all, so it is expanded
# unquoted on purpose: an empty variable must vanish rather than become an
# empty argument.
no_cache=""

# Architectures the elfpak image is published for.
elfpak_platforms="linux/amd64,linux/arm64"
# Platform docker itself runs on, e.g. linux/amd64.
host_platform="$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')"
# Non-empty once a genuinely multi-platform image can be loaded locally.
multi_platform=""

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

# "linux/amd64,linux/arm64" -> "linux/amd64 linux/arm64"
platform_list() { printf '%s' "${1//,/ }"; }

arm64_available() {
    docker run --rm --platform linux/arm64 "$debian_image" true >/dev/null 2>&1
}

# Loading a multi-platform image into the local image store requires the
# containerd image store; without it docker can only hold one platform per tag.
supports_multi_platform_images() {
    docker info --format '{{json .DriverStatus}}' 2>/dev/null \
        | grep -q "containerd.snapshotter"
}

# The tag holding `$1` for `$2`: one shared tag when multi-platform images work,
# a per-platform tag otherwise.
tag_for() {
    local base="$1" platform="$2"
    if [ -n "$multi_platform" ]; then
        printf '%s' "$base"
    else
        printf '%s-%s' "$base" "$(platform_tag "$platform")"
    fi
}

# Build an image for a comma-separated platform list. One buildx invocation
# covers every platform at once; the fallback builds them one tag at a time.
build_image() {
    local dockerfile="$1" platforms="$2" tag="$3"
    shift 3
    if [ -n "$multi_platform" ]; then
        docker buildx build --platform "$platforms" --load -t "$tag" \
            $no_cache "$@" -f "$dockerfile" .
    else
        local platform
        for platform in $(platform_list "$platforms"); do
            docker build --platform "$platform" -t "$(tag_for "$tag" "$platform")" \
                $no_cache "$@" -f "$dockerfile" .
        done
    fi
}

# Export a distribution root filesystem for a platform, to package from.
export_sysroot() {
    local platform="$1" destination="$2" container
    container="$(docker create --platform "$platform" "$debian_image")"
    docker export "$container" | tar -x -C "$destination" 2>/dev/null || true
    docker rm "$container" >/dev/null
}

# The elfpak distribution image. Cross-compiled, so building every supported
# architecture costs about as much as building one.
build_elfpak() {
    local platforms="$elfpak_platforms"
    [ -n "$multi_platform" ] || platforms="$host_platform"
    log "building $elfpak_image for $platforms"
    build_image Dockerfile "$platforms" "$elfpak_image" -q >/dev/null
    docker run --rm --platform "$host_platform" \
        "$(tag_for "$elfpak_image" "$host_platform")" --version
}

# elfpak for a platform, built on demand when multi-platform images are absent.
elfpak_image_for() {
    local platform="$1" tag
    tag="$(tag_for "$elfpak_image" "$platform")"
    if [ -z "$multi_platform" ] && ! docker image inspect "$tag" >/dev/null 2>&1; then
        {
            log "building $tag for $platform"
            docker build --platform "$platform" -q -t "$tag" $no_cache -f Dockerfile . >/dev/null
        } >&2
    fi
    printf '%s' "$tag"
}

# Assert everything the `web` preset promises, against an already-built image.
#
#   $1 platform, $2 image, $3 published port, $4 readiness attempts
check_axum() {
    local platform="$1" image="$2" http_port="$3" attempts="$4"

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
    arch="$(docker image inspect --platform "$platform" "$image" --format '{{.Architecture}}')"
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
    if docker run --rm --platform "$platform" --entrypoint /bin/sh "$image" -c true 2>/dev/null
    then
        fail "the image contains a shell"
    fi
    ok "no shell in the image"

    cleanup
}

# Readiness budget: emulated startup is an order of magnitude slower.
attempts_for() {
    if [ "$1" = "$host_platform" ]; then printf '50'; else printf '300'; fi
}

# Build the Axum fixture for every requested platform, then assert on each.
test_axum() {
    local platforms="${1:-$host_platform}"
    log "building the Axum fixture for $platforms and packaging it with elfpak"
    build_image tests/docker/Dockerfile.axum "$platforms" "$axum_image" \
        --build-arg "ELFPAK_IMAGE=$(elfpak_image_for "$host_platform")"

    local platform index=0
    for platform in $(platform_list "$platforms"); do
        check_axum "$platform" "$(tag_for "$axum_image" "$platform")" \
            "$((port + index))" "$(attempts_for "$platform")"
        index=$((index + 1))
    done
}

test_axum_arm64() {
    if [ "$host_platform" != "linux/arm64" ] && ! arm64_available; then
        skip "linux/arm64 emulation is unavailable"
        return 0
    fi
    test_axum linux/arm64
}

# The same application, bundled with `minimal` instead of `web`, must fail to
# make an HTTPS request: that is what proves the CA roots come from the bundle
# and not from something baked into the binary.
test_ca_policy() {
    local image id base http_port
    image="elfpak-axum:local-minimal"
    http_port="$((port + 8))"

    log "building the same service with --preset minimal"
    build_image tests/docker/Dockerfile.axum "$host_platform" "$image" \
        --build-arg "ELFPAK_IMAGE=$(elfpak_image_for "$host_platform")" \
        --build-arg "ELFPAK_PRESET=minimal"

    id="$(docker run -d --rm --platform "$host_platform" -p "$http_port:8080" \
        "$(tag_for "$image" "$host_platform")")"
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

# A dynamically linked musl program: the loader is libc, reached through a
# symlink, and there is no ld.so.cache anywhere in the picture.
test_musl() {
    local platforms="$host_platform"
    if [ -n "$multi_platform" ] && [ "$host_platform" != "linux/arm64" ] && arm64_available; then
        platforms="$platforms,linux/arm64"
    fi

    log "building a musl program for $platforms and packaging it with elfpak"
    build_image tests/docker/Dockerfile.musl "$platforms" "$musl_image" \
        --build-arg "ELFPAK_IMAGE=$(elfpak_image_for "$host_platform")"

    local platform output
    for platform in $(platform_list "$platforms"); do
        local image
        image="$(tag_for "$musl_image" "$platform")"
        output="$(docker run --rm --platform "$platform" "$image")"
        grep -q "hello from musl" <<<"$output" \
            || fail "the musl program did not run on $platform: $output"
        ok "a dynamically linked musl binary runs from scratch ($platform)"

        grep -q "dns:ok" <<<"$output" || fail "musl name resolution failed: $output"
        ok "musl resolves DNS without any NSS modules ($platform)"

        if docker run --rm --platform "$platform" --entrypoint /bin/sh "$image" -c true 2>/dev/null
        then
            fail "the image contains a shell"
        fi
        ok "no shell in the image ($platform)"
    done
}

# A library in a directory the loader never searches. On the build image it is
# found through /etc/ld.so.cache; a scratch image has one only if elfpak wrote
# it, so this is what proves the generated cache actually works.
test_ld_so_cache() {
    local elfpak_tag image output
    elfpak_tag="$(elfpak_image_for "$host_platform")"

    log "packaging a program whose library lives in /opt/vendor/lib"
    image="$(tag_for "$ldcache_image" "$host_platform")"
    build_image tests/docker/Dockerfile.ldcache "$host_platform" "$ldcache_image" \
        --build-arg "ELFPAK_IMAGE=$elfpak_tag"

    output="$(docker run --rm --platform "$host_platform" "$image")"
    grep -q "vendor value=7" <<<"$output" \
        || fail "the scratch image did not run: $output"
    ok "a library outside every loader directory is found from scratch"

    # The same bundle without the cache must fail, or the cache proves nothing.
    log "building the same bundle with --ld-so-cache=false"
    local without="elfpak-ldcache:local-nocache"
    build_image tests/docker/Dockerfile.ldcache "$host_platform" "$without" \
        --build-arg "ELFPAK_IMAGE=$elfpak_tag" \
        --build-arg "ELFPAK_LD_SO_CACHE=false"

    if docker run --rm --platform "$host_platform" \
        "$(tag_for "$without" "$host_platform")" >/dev/null 2>&1
    then
        fail "the image without a cache should not have started"
    fi
    ok "without the generated cache the same image cannot start"
}

# The tar backend, consumed the way a container build consumes an archive.
#
# The same service as the axum test, packaged with `--tar` and delivered by
# `ADD rootfs.tar /`. `ADD` extracts from the build context only, so the
# archive is exported from the first build and is the context of the second —
# which is what a pipeline does, and what makes this a test of the archive
# rather than of a directory that happens to be tarred.
test_tar() {
    local elfpak_tag work listing
    elfpak_tag="$(elfpak_image_for "$host_platform")"
    work="$(mktemp -d)"
    workdir_to_remove="$work"
    mkdir -p "$work/out"

    log "building the Axum service and packaging it into an archive"
    docker buildx build --platform "$host_platform" $no_cache \
        --target archive --output "type=local,dest=$work/out" \
        --build-arg "ELFPAK_IMAGE=$elfpak_tag" \
        -f tests/docker/Dockerfile.tar .

    [ -f "$work/out/rootfs.tar" ] || fail "no archive was written"
    ok "the archive is exported straight from the build stage"

    grep -q '"tar":' "$work/out/elfpak-manifest.json" \
        || fail "the manifest should record the archive"
    ok "the manifest is written beside the archive, never inside it"

    # Listed once: `tar | grep -q` would hand tar a SIGPIPE and pipefail would
    # then report a failure that never happened.
    listing="$(tar -tf "$work/out/rootfs.tar")"
    grep -qx 'app/server' <<<"$listing" || fail "the archive has no /app/server"
    grep -qx 'etc/ssl/certs/ca-certificates.crt' <<<"$listing" \
        || fail "the web preset should have contributed a CA bundle"
    ok "the archive contains the install path and what the preset added"

    if grep -q '^/' <<<"$listing"; then
        fail "archive entries must be relative, or ADD would write outside /"
    fi
    ok "every entry is a relative path"

    log "building a scratch image with ADD"
    # Always single-platform, so the plain tag is the whole story.
    docker build --platform "$host_platform" -q -t "$tar_image" $no_cache \
        --target runtime -f tests/docker/Dockerfile.tar "$work/out" >/dev/null
    ok "docker ADD unpacks the archive into a scratch image"

    check_axum "$host_platform" "$tar_image" "$((port + 9))" \
        "$(attempts_for "$host_platform")"
}

# `elfpak verify` as a build gate.
#
# Dockerfile.verify bundles in one stage, verifies against the manifest in the
# next, and copies into the image from the stage that verified. The middle
# stage checks the positive space (this bundle verifies) and the negative space
# (changed bytes, changed mode, added file, removed file, redirected symlink),
# so a passing build means all of it held.
test_verify() {
    local elfpak_tag image output
    elfpak_tag="$(elfpak_image_for "$host_platform")"
    image="$(tag_for "$verify_image" "$host_platform")"

    log "bundling, verifying and shipping in one build"
    build_image tests/docker/Dockerfile.verify "$host_platform" "$verify_image" \
        --build-arg "ELFPAK_IMAGE=$elfpak_tag" \
        --build-arg "DEBIAN_IMAGE=$debian_image"
    ok "the bundle verifies in the stage between packaging and shipping"

    docker run --rm --platform "$host_platform" "$image" -la /app >/dev/null \
        || fail "the verified image did not run"
    ok "the rootfs that ships is the rootfs that was verified, and it runs"

    # A gate that cannot fail proves nothing, so make it fail.
    log "corrupting the same rootfs before it reaches verification"
    if output="$(build_image tests/docker/Dockerfile.verify "$host_platform" \
        "elfpak-verify:local-tampered" \
        --build-arg "ELFPAK_IMAGE=$elfpak_tag" \
        --build-arg "DEBIAN_IMAGE=$debian_image" \
        --build-arg "ELFPAK_TAMPER=1" 2>&1)"
    then
        fail "a build whose rootfs was corrupted should not have succeeded"
    fi
    grep -q 'E5001' <<<"$output" \
        || fail "the build failed, but not because verification failed"
    ok "a corrupted rootfs fails the build instead of shipping"
}

# Package a foreign-architecture binary from an exported sysroot. Nothing is
# compiled and nothing is executed for the target architecture.
test_cross() {
    if ! arm64_available; then
        skip "linux/arm64 emulation is unavailable"
        return 0
    fi
    local elfpak_tag work
    elfpak_tag="$(elfpak_image_for "$host_platform")"

    log "exporting an aarch64 sysroot"
    work="$(mktemp -d)"
    workdir_to_remove="$work"
    mkdir -p "$work/sysroot" "$work/out"
    export_sysroot linux/arm64 "$work/sysroot"

    log "packaging an aarch64 binary with the $(platform_tag "$host_platform") elfpak"
    docker run --rm --platform "$host_platform" \
        -v "$work/sysroot:/sysroot:ro" \
        -v "$work/out:/out" \
        "$elfpak_tag" bundle /sysroot/bin/ls \
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
    docker build --platform linux/arm64 -q -t "$cross_image" $no_cache "$work/out" >/dev/null
    docker run --rm --platform linux/arm64 "$cross_image" -la / >/dev/null \
        || fail "the aarch64 image did not run"
    ok "aarch64 scratch image runs"

    cleanup
}

# Every architecture the suite covers, in one build per image.
all_platforms() {
    if [ -n "$multi_platform" ] && [ "$host_platform" != "linux/arm64" ] && arm64_available; then
        printf '%s,linux/arm64' "$host_platform"
    else
        printf '%s' "$host_platform"
    fi
}

usage() {
    cat <<'TEXT'
usage: tests/docker/smoke.sh [--fresh] [test]

tests:
  axum        Axum on scratch, host architecture (default: all of them)
  axum-arm64  Axum on scratch, linux/arm64
  ca          CA roots come from the bundle, not the binary
  musl        a dynamically linked musl program
  ldcache     a library the loader only finds through a cache
  tar         the same service delivered as a tar and ADDed
  verify      `elfpak verify` as a build gate
  cross       non-Rust cross-architecture packaging

options:
  --fresh     remove the images this suite owns and build everything with
              --no-cache, so nothing in a rerun comes from a previous one
  --help      this text
TEXT
}

# Every image this suite creates, and nothing else: `elfpak:local*` for the
# tool, `elfpak-<test>:local*` for the fixtures.
remove_suite_images() {
    local ids
    ids="$(docker image ls -q \
        --filter 'reference=elfpak:local*' \
        --filter 'reference=elfpak-*:local*' | sort -u)"
    if [ -z "$ids" ]; then
        return 0
    fi
    # shellcheck disable=SC2086 # one argument per image id, deliberately split.
    docker rmi -f $ids >/dev/null 2>&1 || true
}

fresh=""
test_to_run="all"
while [ $# -gt 0 ]; do
    case "$1" in
        --fresh|--no-cache) fresh=1; no_cache="--no-cache" ;;
        -h|--help)          usage; exit 0 ;;
        -*)                 usage >&2; fail "unknown option: $1" ;;
        *)                  test_to_run="$1" ;;
    esac
    shift
done

if supports_multi_platform_images; then
    multi_platform=1
else
    skip "the containerd image store is unavailable, building one platform per tag"
fi

if [ -n "$fresh" ]; then
    log "removing the images of previous runs"
    remove_suite_images
    ok "every image below is built from nothing"
fi

build_elfpak

case "$test_to_run" in
    axum)       test_axum ;;
    axum-arm64) test_axum_arm64 ;;
    ca)         test_ca_policy ;;
    musl)       test_musl ;;
    ldcache)    test_ld_so_cache ;;
    tar)        test_tar ;;
    verify)     test_verify ;;
    cross)      test_cross ;;
    all)
        test_axum "$(all_platforms)"
        test_ca_policy
        test_musl
        test_ld_so_cache
        test_tar
        test_verify
        test_cross
        ;;
    *)          usage >&2; fail "unknown test: $test_to_run" ;;
esac

log "all smoke tests passed"
