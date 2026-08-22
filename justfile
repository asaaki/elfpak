# Everything the project checks about itself, in one place.
default:
    @just --list

# Bump the workspace version, commit it, create an annotated tag, and push it.
bump kind:
    #!/usr/bin/env bash
    set -euo pipefail

    bump_kind="{{ kind }}"
    case "$bump_kind" in
        major|minor|patch) ;;
        *)
            echo "usage: just bump <major|minor|patch>" >&2
            exit 2
            ;;
    esac

    if [[ "$(git branch --show-current)" != "main" ]]; then
        echo "release bumps must be made from the main branch" >&2
        exit 1
    fi
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "release bumps require a clean worktree" >&2
        exit 1
    fi
    git remote get-url origin >/dev/null
    if ! command -v tomli >/dev/null; then
        echo "release bumps require tomli (cargo binstall tomli)" >&2
        exit 1
    fi

    current="$(tomli query --strip-trailing-newline --filepath Cargo.toml workspace.package.version | tr -d '[:space:]\"')"
    if [[ ! "$current" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
        echo "workspace version is missing or is not simple SemVer: ${current:-<missing>}" >&2
        exit 1
    fi
    core_dependency_version="$(tomli query --strip-trailing-newline --filepath Cargo.toml workspace.dependencies.elfpak-core.version | tr -d '[:space:]\"')"
    if [[ "$core_dependency_version" != "$current" ]]; then
        echo "elfpak-core dependency version ${core_dependency_version:-<missing>} does not match workspace version $current" >&2
        exit 1
    fi

    major="${BASH_REMATCH[1]}"
    minor="${BASH_REMATCH[2]}"
    patch="${BASH_REMATCH[3]}"
    case "$bump_kind" in
        major) new_version="$((major + 1)).0.0" ;;
        minor) new_version="$major.$((minor + 1)).0" ;;
        patch) new_version="$major.$minor.$((patch + 1))" ;;
    esac

    tag="v$new_version"
    if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
        echo "tag $tag already exists" >&2
        exit 1
    fi

    manifest_next="$(mktemp --tmpdir=. .Cargo.toml.XXXXXX)"
    trap 'rm -f "$manifest_next"' EXIT
    tomli set --strip-trailing-newline workspace.package.version "$new_version" < Cargo.toml |
        tomli set --strip-trailing-newline workspace.dependencies.elfpak-core.version "$new_version" > "$manifest_next"
    chmod --reference=Cargo.toml "$manifest_next"
    mv "$manifest_next" Cargo.toml
    trap - EXIT
    cargo update --workspace --offline

    git add Cargo.toml Cargo.lock
    git commit -S -m "Release $tag"
    git tag -s "$tag" -m "Release $tag"
    git push origin main --follow-tags

    echo "Released $tag"

# Format, lint at the strictest setting, and run every test.
check: fmt-check lint test

# Rewrite every file the way rustfmt wants it (100 columns, 4 spaces).
fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# Reflow the `use` blocks: one sorted block per file, merged per crate. Both
# options are nightly-only, which is why they are not in rustfmt.toml.
imports:
    cargo +nightly fmt --all -- --config group_imports=One,imports_granularity=Crate
    cargo +nightly fmt --manifest-path fuzz/Cargo.toml --all -- \
        --config group_imports=One,imports_granularity=Crate

# Clippy at its strictest; a warning fails the recipe.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Unit, integration, and loader-oracle tests.
test:
    cargo test --workspace

# Docker smoke tests: a test name, --fresh, or nothing (see DOCUMENTATION.md).
smoke *args:
    tests/docker/smoke.sh {{args}}

# Parser fuzzing. Needs a nightly toolchain and cargo-fuzz.
fuzz target="parse_elf" seconds="60":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}
