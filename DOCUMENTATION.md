# elfpak documentation

Reference for `elfpak`. See [README.md](README.md) for the short version.

- [Commands](#commands)
- [Presets and runtime policy](#presets-and-runtime-policy)
- [Configuration file](#configuration-file)
- [Dependency policy](#dependency-policy)
- [Manifest and `verify`](#manifest-and-verify)
- [How dependencies are resolved](#how-dependencies-are-resolved)
- [Cross-architecture packaging](#cross-architecture-packaging)
- [Guarantees and determinism](#guarantees-and-determinism)
- [Repository layout](#repository-layout)
- [Development](#development)
- [Status](#status)

## Commands

```text
elfpak inspect <binary> [--root <sysroot>] [--library-path <dir>] [--json]

elfpak bundle <binary>
    --output <dir>            where the rootfs is written
    --install <path>          path of the executable inside the rootfs
    --root <sysroot>          logical / used for dependency lookup (default: /)
    --preset <minimal|web>    runtime policy preset
    --include <path>          extra file or directory, location preserved
    --allow-library <soname>  dependency allow-list (repeatable)
    --user <uid[:gid]>        identity the application runs as
    --library-path <dir>      extra search directory, like LD_LIBRARY_PATH
    --ca-certificates[=BOOL] --tmp[=BOOL] --passwd-group[=BOOL]
    --nsswitch[=BOOL] --tzdata[=BOOL]
    --manifest <path> | --no-manifest
    --dry-run --clean --config <file> --no-config

elfpak verify <manifest> [--rootfs <dir>]
```

Global flags: `-q/--quiet`, `-v`/`-vv` for verbosity. `-v` on `bundle` prints
every planned file with the reason it was included.

`inspect` and `bundle --dry-run` perform the complete discovery, resolution and
planning work without touching the output filesystem.

### `inspect`

```console
$ elfpak inspect ./server
./server
  ELF64 LSB x86_64

  interpreter:
    /lib64/ld-linux-x86-64.so.2
      -> /usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2

  dependencies:
    libgcc_s.so.1
      /usr/lib/x86_64-linux-gnu/libgcc_s.so.1

    libc.so.6
      /usr/lib/x86_64-linux-gnu/libc.so.6

  runtime:
    3 shared objects
    7.7 MiB

  warnings:
    none
```

`--json` emits the same plan in the manifest format, for scripting.

### `bundle`

```console
$ elfpak bundle ./server \
    --output /out/rootfs \
    --install /app/server \
    --preset web \
    --user 65532:65532
```

The executable is installed at `--install`; every other file keeps the path it
had in the source root. The manifest is written beside the rootfs, never inside
it (`--manifest` overrides the location, `--no-manifest` disables it).

## Presets and runtime policy

ELF analysis can prove which shared objects a program loads. It cannot prove
which *data* files it needs, so those are modeled separately as runtime policy.

| Preset    | Contents |
|-----------|----------|
| `minimal` | the ELF closure only: application, `PT_INTERP`, recursive `DT_NEEDED` |
| `web`     | the ELF closure plus CA certificates, `/tmp`, `passwd`/`group`, `nsswitch.conf` |

Presets are convenience configuration, never hidden behaviour. Every feature is
also independently switchable, and the preset only supplies the defaults:

```console
$ elfpak bundle ./server -o /rootfs --preset web --tzdata --tmp=false
```

* `--ca-certificates` — the system CA bundle, found at its usual location and
  copied there, with its symlink chain intact.
* `--tmp` — an empty `/tmp` with mode `1777`.
* `--passwd-group` — generated `/etc/passwd` and `/etc/group` containing `root`,
  `nobody` and the `--user` identity. Generated rather than copied, so the
  output stays deterministic and leaks nothing from the build host.
* `--nsswitch` — a generated `/etc/nsswitch.conf`, plus the glibc NSS modules
  (`libnss_files`, `libnss_dns`, `libresolv`) when the source root still ships
  them. Since glibc 2.34 they are built into `libc.so.6`.
* `--tzdata` — `/usr/share/zoneinfo` and `/etc/localtime`. Opt-in in every
  preset, because most services do not need it.
* `--include <path>` — any other file or directory, copied to the same absolute
  path. Directories are copied recursively and symlinks inside them are
  reproduced as symlinks.

With `web`, an application that performs outbound HTTPS needs no CA-specific
code: the system trust store is exactly where the TLS stack already looks.
Pinning a bundle explicitly in application code remains possible, but it is
opt-in rather than a requirement.

`--user` only records an identity in `passwd`/`group`. The container runtime
still decides who the process runs as (`USER 65532:65532` in the Dockerfile).

## Configuration file

`elfpak.toml` is optional and is picked up from the working directory. CLI
arguments always win; `--config <file>` selects another file and `--no-config`
ignores it entirely.

```toml
[package]
binary = "/my-server"
install = "/app/server"
output = "/rootfs"
root = "/"

[runtime]
preset = "web"
user = "65532:65532"
tzdata = false

[include]
paths = ["/app/templates"]

[dependencies]
allow = ["libc.so.6", "libgcc_s.so.1"]
```

Unknown keys are rejected rather than ignored, so typos surface immediately.

## Dependency policy

`[dependencies].allow` (or `--allow-library`, repeatable) turns the runtime
closure into a contract. A new native dependency then fails the build instead of
silently growing the image, which is what makes it useful in CI:

```text
error[E2002]:
  library `libssl.so.3` is not allowed by dependency policy

required by:
  /app/server

add:
  --allow-library libssl.so.3
```

Libraries match on `DT_SONAME` or on file name. The ELF interpreter is always
allowed; it is not a `DT_NEEDED` dependency.

## Manifest and `verify`

Every bundle writes `elfpak-manifest.json` beside the rootfs. It records each
file, its digest, its mode, and why it is there:

```json
{
  "binary": "/app/server",
  "architecture": "x86_64",
  "interpreter": "/lib64/ld-linux-x86-64.so.2",
  "files": [
    { "path": "/app/server", "reason": "application", "sha256": "…" },
    {
      "path": "/usr/lib/x86_64-linux-gnu/libc.so.6",
      "reason": { "needed_by": "/app/server", "soname": "libc.so.6" },
      "sha256": "…"
    }
  ]
}
```

Reasons are one of `application`, `interpreter`, `include`,
`{ needed_by, soname }` or `{ runtime_policy }`. That makes bundles auditable
and diffable, and is the basis for future SBOM output.

```console
$ elfpak verify /out/elfpak-manifest.json
ok: 25 entries verified in /out/rootfs
```

`verify` checks that every entry exists, that regular files hash as recorded,
and that symlinks still point where they did. It needs no Docker. Pass
`--rootfs` to check a tree other than the one recorded in the manifest.

## How dependencies are resolved

`elfpak` models the glibc loader rather than searching for matching filenames:

* `PT_INTERP` and recursive `DT_NEEDED`
* `DT_RPATH`, inherited down the loading chain
* `DT_RUNPATH`, which — matching the loader — is *not* inherited and takes
  precedence over `DT_RPATH` on the object that declares it
* `$ORIGIN`, `$LIB` and `$PLATFORM` token expansion
* `/etc/ld.so.cache`, parsed directly in both the historical and the current
  format; `ldconfig` is never invoked
* `/etc/ld.so.conf`, including `include` globs
* architecture-specific default directories and `glibc-hwcaps` subdirectories
* `DF_1_NODEFLIB`
* architecture, ELF class and endianness validation of every candidate, so a
  file that merely has the right name can never satisfy a lookup

Original paths and symlink structure are preserved: `libfoo.so.1 ->
libfoo.so.1.4.2` stays a symlink, `/lib -> usr/lib` stays a symlink, and
libraries are never relocated into a private directory with a compensating
`LD_LIBRARY_PATH`. The generated rootfs keeps the original loader contract.

`dlopen` cannot be followed statically. When an object references it, `elfpak`
warns and continues; use `--include` for anything loaded at runtime.

## Cross-architecture packaging

The source filesystem is abstracted behind `--root`, which the resolver treats
as the logical `/`. An x86_64 `elfpak` can therefore package an aarch64
application from an aarch64 sysroot without executing anything:

```console
$ elfpak bundle /sysroot/app/server \
    --root /sysroot \
    --output /rootfs \
    --install /app/server
```

Supported target architectures are x86_64 and aarch64.

## Guarantees and determinism

`elfpak bundle`:

* does not execute the target
* does not call `ldd` or `ldconfig`
* does not execute shell commands
* does not contact the network
* does not invoke Docker
* treats the source filesystem as read-only
* writes only beneath `--output`
* records every included file and why

Output is deterministic for the same application binary, source root,
configuration and `elfpak` version: entries are ordered by destination, file
modes are normalized to `0755`/`0644`, and timestamps are pinned to
`SOURCE_DATE_EPOCH` (default: the Unix epoch).

Path handling is defensive throughout: destinations are normalized lexically,
`..` can never escape the output root, and writes through a symlinked parent are
refused.

## Repository layout

```text
crates/elfpak/        CLI: argument parsing, config loading, rendering
crates/elfpak-core/   library: elf, graph, resolver, plan, rootfs, manifest
fixtures/axum-server/ integration fixture: a real Axum service
tests/docker/         Docker smoke tests
Dockerfile            static elfpak distribution image (FROM scratch)
```

`elfpak-core` holds the reusable implementation; no resolution logic lives in
the CLI crate. Base images are pinned by tag and digest.

## Development

Minimum supported Rust version: **1.97**.

```console
$ cargo test                       # unit + integration tests
$ cargo clippy --all-targets
$ tests/docker/smoke.sh            # all Docker smoke tests
$ tests/docker/smoke.sh axum       # Axum on scratch, host architecture
$ tests/docker/smoke.sh axum-arm64 # Axum on scratch, linux/arm64
$ tests/docker/smoke.sh ca         # CA roots come from the bundle, not the binary
$ tests/docker/smoke.sh cross      # non-Rust cross-architecture packaging
```

The cargo suite covers ELF parsing, token expansion, `ld.so.cache` parsing,
policy evaluation, manifest round-trips and filesystem safety, plus loader
semantics against a synthetic sysroot of purpose-built C fixtures (transitive
`DT_NEEDED`, RPATH inheritance versus RUNPATH non-inheritance, `$ORIGIN`,
`ld.so.conf` globs, cache-only libraries, decoy files, symlinked DSOs).

It also compares against the real glibc loader: for host binaries, `elfpak`'s
closure must equal what `ldd` reports. `ldd` is a test oracle only; the tool
itself never runs it.

The Docker smoke tests:

* `axum` — builds the Axum fixture, packages it, and asserts that the
  `FROM scratch` image starts, binds an unprivileged port, resolves DNS,
  performs outbound HTTPS, writes to `/tmp`, runs as uid 65532, and contains no
  shell. The HTTPS check uses a plain client with no CA configuration in the
  application at all.
* `ca` — bundles the same binary with `--preset minimal` and asserts that it
  still starts but can no longer make an HTTPS request, which is what proves the
  trust roots come from the bundle rather than from the application.
* `axum-arm64` — the same end-to-end test on `linux/arm64`: the service, the
  `elfpak` that packages it, and the resulting scratch image are all aarch64. On
  an x86_64 host everything compiles under qemu, so it is slow.
* `cross` — exports an aarch64 sysroot, packages an aarch64 binary with the host
  `elfpak`, and runs the result under emulation. Nothing is compiled or executed
  for the foreign architecture, which keeps it cheap and covers the non-Rust
  path.

## Status

Implemented (roadmap 0.1/0.2): workspace, CLI, ELF parsing, architecture
detection, full static closure, RPATH/RUNPATH/token/cache/conf resolution, path
and symlink preservation, presets, manifest, hashes, dependency allow-list,
`verify`, deterministic output, and x86_64 + aarch64 support.

Not implemented yet, by design: tar and OCI output, runtime tracing
(`elfpak trace`), SBOM generation, and musl-specific behaviour beyond generic
ELF parsing.
