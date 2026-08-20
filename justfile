# Everything the project checks about itself, in one place.
default:
    @just --list

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
