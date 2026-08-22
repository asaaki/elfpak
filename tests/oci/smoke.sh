#!/usr/bin/env bash
set -euo pipefail

for tool in skopeo podman; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "OCI smoke test requires $tool" >&2
        exit 1
    fi
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
smoke_dir="$(mktemp -d)"
trap 'rm -rf "$smoke_dir"' EXIT

case "$(uname -m)" in
    x86_64) expected_arch="amd64" ;;
    aarch64|arm64) expected_arch="arm64" ;;
    *)
        echo "OCI smoke test does not map host architecture $(uname -m)" >&2
        exit 1
        ;;
esac

cd "$repo_root"
cargo build -p elfpak

oci_dir="$smoke_dir/elfpak.oci"
oci_tar="$smoke_dir/elfpak.oci.tar"
target/debug/elfpak bundle target/debug/elfpak \
    --oci-layout "$oci_dir" \
    --oci-archive "$oci_tar" \
    --image-tag smoke \
    --install /elfpak \
    --entrypoint /elfpak \
    --no-config \
    --no-manifest

dir_arch="$(skopeo inspect --format '{{.Architecture}}' "oci:${oci_dir}:smoke")"
tar_arch="$(skopeo inspect --format '{{.Architecture}}' "oci-archive:${oci_tar}:smoke")"
if [[ "$dir_arch" != "$expected_arch" || "$tar_arch" != "$expected_arch" ]]; then
    echo "OCI architecture mismatch: expected $expected_arch, layout=$dir_arch, archive=$tar_arch" >&2
    exit 1
fi

skopeo inspect "oci:${oci_dir}:smoke" >/dev/null
skopeo inspect "oci-archive:${oci_tar}:smoke" >/dev/null
podman run --rm "oci-archive:${oci_tar}:smoke" --version
