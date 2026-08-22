# elfpak

Analyze a Linux ELF application, determine its runtime closure the way the glibc
loader would, and package that closure for a `FROM scratch` container.

> `cargo vendor`, but for an executable's Linux runtime.

`elfpak` is a Rust replacement for [`magicpak`](https://github.com/coord-e/magicpak),
narrowly focused on turning a compiled binary plus the filesystem it was built
against into a deterministic minimal rootfs.

```mermaid
flowchart TB
    Build["cargo build"] --> Binary["ELF binary"]
    Binary --> Bundle["elfpak bundle"]
    Bundle --> Rootfs["minimal rootfs directory"]
    Bundle --> Tar["deterministic rootfs tar"]
    Bundle --> Manifest["manifest"]
    Rootfs --> Scratch["FROM scratch"]
    Tar --> Scratch
    Rootfs --> Verify["elfpak verify"]
    Manifest --> Verify
```

## Quick start

```dockerfile
# syntax=docker/dockerfile:1

FROM ghcr.io/asaaki/elfpak:0.1 AS elfpak

FROM rust:1.98.0-slim-trixie AS build
WORKDIR /src
COPY --from=elfpak /elfpak /usr/local/bin/elfpak
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && cp target/release/my-server /my-server
RUN elfpak bundle /my-server \
        --output /rootfs \
        --install /app/server \
        --preset web \
        --user 65532:65532

FROM scratch
COPY --from=build /rootfs /
USER 65532:65532
WORKDIR /app
ENTRYPOINT ["/app/server"]
```

The resulting image contains the application, its ELF closure, and whatever the
runtime policy asked for.

## Cargo workflow

Install the Cargo adapter when packaging binaries from a Rust project:

```console
$ cargo install cargo-elfpak
$ cargo elfpak bundle --release \
    --output rootfs \
    --install /app/server \
    --preset web
```

`cargo-elfpak` asks Cargo to build the selected binaries. Cargo reuses each one
when it is fresh and rebuilds it when any tracked input changed; the exact
executable paths Cargo reports are passed to the normal `elfpak bundle`
implementation.
Use `-p <package>` in an ambiguous workspace and `--bin <name>` when the package
has no inferable default binary. Multi-binary projects can select a subset with
`-p <package> --bins server,migrate`, every binary in one package with
`-p <package> --all-bins`, or every binary in the workspace with `--all`; use
`--install-dir` to preserve their names under one directory:

```console
$ cargo elfpak bundle --release --all \
    --output rootfs --install-dir /app --preset web
```

## Commands

```text
elfpak inspect <binary>    analyze and print the runtime closure, copying nothing
elfpak bundle  <binary>... build a minimal rootfs plus a manifest
elfpak verify  <manifest>  check a materialized rootfs against its manifest
```

`bundle` writes a directory (`--output`), a deterministic tar archive
(`--tar`, for `ADD rootfs.tar /`), or both from the same plan.

Two presets: `minimal` is the ELF closure alone, `web` adds CA certificates,
`/tmp`, `passwd`/`group` and `nsswitch.conf`. Every feature is also switchable
on its own, and an optional `elfpak.toml` can supply defaults.

A service packaged with `--preset web` does DNS and outbound HTTPS without any
CA-specific code in the application; the system trust store comes along.

## What makes it different

* **Loader semantics, not filename matching.** `PT_INTERP`, recursive
  `DT_NEEDED`, `DT_RPATH` inheritance versus `DT_RUNPATH`, `$ORIGIN`/`$LIB`/
  `$PLATFORM`, `ld.so.cache`, `ld.so.conf`, deliberate exclusion of unsafe
  CPU-specific glibc-hwcaps variants, and architecture validation of every
  candidate.
* **Original paths and symlinks preserved.** `libfoo.so.1 -> libfoo.so.1.4.2`
  stays a symlink; nothing is relocated into a private directory with a
  compensating `LD_LIBRARY_PATH`. Where a library sits outside the directories
  the loader searches, the bundle gets a generated `/etc/ld.so.cache` instead —
  a real one, written from the plan, because `ldconfig` is never run.
* **Every file has a recorded reason.** The manifest beside the rootfs says what
  was included and why, along with the policy it was built with; `elfpak verify`
  re-checks it, and `--strict` also rejects anything that was added afterwards
  or whose permissions changed.
* **An allow-list turns dependencies into a contract.** A new native dependency
  fails the build instead of silently growing the image.
* **Cross-architecture.** `--root` abstracts the source filesystem, so an x86_64
  `elfpak` can package an aarch64 application from an aarch64 sysroot.

## Guarantees

`elfpak bundle` does not execute the target, does not call `ldd` or `ldconfig`,
does not run shell commands, does not contact the network, and does not invoke
Docker. It treats the source filesystem as read-only and writes only to the
requested directory, tar and manifest destinations and their temporary
siblings.

Output is deterministic for the same binaries, source root, configuration and
`elfpak` version.

Directory, tar and manifest outputs are staged beside their destinations and
published only when complete, so a failed build leaves the previous artifact
intact instead of exposing partial output.

## Documentation

[DOCUMENTATION.md](DOCUMENTATION.md) covers the full CLI, runtime policy,
configuration file, dependency policy, manifest format, resolver behaviour,
cross-architecture packaging and the test suite.

## Development

```console
$ just check              # fmt, clippy -D warnings, and the whole test suite
$ just test               # unit, integration and loader-oracle tests
$ just smoke              # Docker smoke tests (see DOCUMENTATION.md)
$ just smoke --fresh      # ... with nothing reused from a previous run

$ cargo run -p cargo-elfpak -- bundle --help

$ docker buildx build --platform linux/amd64,linux/arm64 -t elfpak:local --load .
```

The design is inspired by [TigerStyle](https://tigerstyle.dev/): safety first,
bounded work, explicit invariants, deterministic output, and performance that
does not come at the cost of readable Rust. [STYLE.md](STYLE.md) records the
project's adaptation without imposing mechanical line-count rules.

The distribution image is multi-platform and cross-compiled, so building every
architecture never needs emulation.

## Status

Roadmap 0.1/0.2 is implemented for x86_64 and aarch64, along with tar output,
loader-oracle tests against real glibc, and parser fuzzing. OCI output, runtime
tracing (`elfpak trace`) and SBOM generation are not implemented yet.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
