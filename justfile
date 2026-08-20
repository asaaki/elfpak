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

# Warnings are bugs that have not happened yet.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Unit, integration, loader-oracle and style tests.
test:
    cargo test --workspace

# The numeric style limits on their own: 100 columns, 70 lines per function.
style:
    cargo test -p elfpak-core --test style

# Docker smoke tests: a test name, --fresh, or nothing (see DOCUMENTATION.md).
smoke *args:
    tests/docker/smoke.sh {{args}}

# Parser fuzzing. Needs a nightly toolchain and cargo-fuzz.
fuzz target="parse_elf" seconds="60":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}
