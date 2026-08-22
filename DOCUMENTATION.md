# elfpak documentation

Reference for `elfpak`. See [README.md](README.md) for the short version.

- [Commands](#commands)
- [Presets and runtime policy](#presets-and-runtime-policy)
- [Configuration file](#configuration-file)
- [Dependency policy](#dependency-policy)
- [Manifest and `verify`](#manifest-and-verify)
- [How dependencies are resolved](#how-dependencies-are-resolved)
  - [The generated loader cache](#the-generated-loader-cache)
- [Cross-architecture packaging](#cross-architecture-packaging)
- [Guarantees and determinism](#guarantees-and-determinism)
- [Repository layout](#repository-layout)
- [Development](#development)
- [Status](#status)

## Commands

```text
elfpak inspect <binary> [--root <sysroot>] [--library-path <dir>] [--json]

elfpak bundle <binary>
    --output <dir>            where the rootfs directory is written
    --tar <file>              where the rootfs tar archive is written
    --install <path>          path of the executable inside the rootfs
    --root <sysroot>          logical / used for dependency lookup (default: /)
    --preset <minimal|web>    runtime policy preset
    --include <path>          extra file or directory, location preserved
    --allow-library <soname>  dependency allow-list (repeatable)
    --user <uid[:gid]>        identity the application runs as
    --library-path <dir>      extra search directory, like LD_LIBRARY_PATH
    --ca-certificates[=BOOL] --tmp[=BOOL] --passwd-group[=BOOL]
    --nsswitch[=BOOL] --tzdata[=BOOL] --ld-so-cache[=BOOL]
    --manifest <path> | --no-manifest
    --dry-run --clean --config <file> --no-config

elfpak verify <manifest> [--rootfs <dir>] [--strict]
```

At least one of `--output` and `--tar` is required; giving both writes both from
the same plan.

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

### Tar output

`--tar` writes a tar archive instead of, or in addition to, a directory:

```console
$ elfpak bundle ./server --tar /out/rootfs.tar --install /app/server --preset web
```

```dockerfile
FROM scratch
ADD rootfs.tar /
ENTRYPOINT ["/app/server"]
```

The archive is written from the bundle plan, not from a materialized directory,
so both backends describe exactly the same tree. It is deterministic by
construction: entries follow plan order, ownership is `0:0`, timestamps come
from `SOURCE_DATE_EPOCH` (default: the Unix epoch), modes are the normalized
ones from the plan, and paths are relative. The same plan produces a
byte-identical archive on every run.

Symlinks are stored as symlink entries and directories keep their modes,
including the sticky bit on `/tmp`.

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
* `--ld-so-cache` — a generated `/etc/ld.so.cache`. Unlike the others this one
  defaults to *automatic*: a cache is written exactly when the closure contains
  something the loader could not otherwise find (see
  [The generated loader cache](#the-generated-loader-cache)). `--ld-so-cache`
  always writes one, `--ld-so-cache=false` never does.

With `web`, an application that performs outbound HTTPS needs no CA-specific
code: the system trust store is exactly where the TLS stack already looks.
Pinning a bundle explicitly in application code remains possible, but it is
opt-in rather than a requirement.

`--user` only records an identity in `passwd`/`group`. The container runtime
still decides who the process runs as (`USER 65532:65532` in the Dockerfile).

### When two features want the same path

Every entry is planned before anything is written, so a destination two features
both claim is settled once, in the plan. Directory scaffolding never displaces
real content, and content always replaces scaffolding. Between two entries that
both carry content, the phases decide, in order of how little choice there was
about the path: the ELF closure first, whose objects must sit exactly where the
loader will look for them, then runtime policy, then `--include`. A generated
`/etc/passwd` therefore keeps its place against an `--include` of the source
root's `/etc`, and `elfpak bundle -v` lists the winner with its reason.

The one case that is an error rather than a precedence is `--install` landing on
a library the closure needs at that exact path: the bundle would be left unable
to load it, so `E4001` reports the collision instead.

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
# ld_so_cache = true   # force a loader cache; omit to let the closure decide

[include]
paths = ["/app/templates"]

[dependencies]
allow = ["libc.so.6", "libgcc_s.so.1"]
```

Unknown keys are rejected rather than ignored, so typos surface immediately.

## Dependency policy

`[dependencies].allow` (or `--allow-library`, repeatable) turns the runtime
closure into a contract. A new native dependency then fails the build instead of
silently growing the image, which is why it belongs in CI:

```text
error[E2002]:
  library `libssl.so.3` is not allowed by dependency policy

required by:
  /app/server

add:
  --allow-library libssl.so.3
```

Libraries match on `DT_SONAME` or on file name.

The allow-list covers the application's *own* ELF closure — what a new `use` or
`#include` in the source would add. Two things are deliberately outside it,
because no caller could name them up front: the ELF interpreter, which is not a
`DT_NEEDED` dependency, and anything runtime policy pulled in, such as the NSS
modules `--nsswitch` adds when the source root still ships them.

## Manifest and `verify`

Every bundle writes `elfpak-manifest.json` beside the rootfs. It records each
file, its digest, its mode, and why it is there:

```json
{
  "binary": "/app/server",
  "architecture": "x86_64",
  "interpreter": "/lib64/ld-linux-x86-64.so.2",
  "policy": {
    "preset": "web",
    "ca_certificates": true,
    "tmp": true,
    "passwd_group": true,
    "nsswitch": true,
    "tzdata": false,
    "ld_so_cache": "auto",
    "user": "app:65532:65532"
  },
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

The `policy` object records the *resolved* runtime and dependency policy —
preset, every runtime feature, the user identity, explicit includes and the
allow-list. Reproducing a bundle requires the same configuration, so the
configuration is part of the record rather than something the caller has to
remember.

```console
$ elfpak verify /out/elfpak-manifest.json
ok: 25 entries verified in /out/rootfs
```

`verify` checks that every entry exists, that regular files hash as recorded,
and that symlinks still point where they did. It needs no Docker. Pass
`--rootfs` to check a tree other than the one recorded in the manifest.

By default that proves nothing was **removed or altered**. `--strict` also walks
the rootfs and fails on anything the manifest does not list, so files *added*
after the bundle was built are caught too, and compares permission bits, which a
content digest cannot see (a file that became setuid still hashes the same):

```console
$ elfpak verify /out/elfpak-manifest.json --strict
  /opt/payload/extra.so: present in the rootfs but not listed in the manifest
error[E5001]:
  verification failed: 1 problem(s) across 25 manifest entries
```

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

### The generated loader cache

The loader finds a library in one of three ways: a directory the object itself
names (`DT_RPATH`/`DT_RUNPATH`), a directory built into the loader
(`/lib`, `/usr/lib` and the architecture variants), or `/etc/ld.so.cache`. On a
normal system `ldconfig` maintains that cache, and it is how a library in
`/usr/local/lib` or `/opt/…/lib` is found at all.

A bundle has no `ldconfig` — and copying the build host's cache would describe
the host's filesystem, not the bundle's. So `elfpak` writes the cache itself,
straight from the plan:

```console
$ elfpak bundle ./server -o /rootfs --install /app/server --library-path /opt/vendor/lib -v
    - /etc/ld.so.cache                       runtime policy: ld-so-cache
    ...
```

It is a real `glibc-ld.so.cache1.1` image: every shared object in the bundle,
mapped from its `DT_SONAME` to the path it occupies in the rootfs, with the
architecture's cache flags, in the descending order glibc's binary search
expects. The test suite checks it against the real loader rather than against
`elfpak`'s own reading of it — a fixture whose library sits outside every
default directory is packaged, `chroot`ed into, and run.

Because the cache is generated rather than copied it stays deterministic, it is
recorded in the manifest like any other file, and it only lists libraries that
are actually in the bundle.

By default one is written only when it is needed, so a service whose libraries
all live in `/usr/lib/<tuple>` — the common case — gets exactly the same image
as before. `--ld-so-cache` forces one; `--ld-so-cache=false` suppresses it and
turns the situation back into the warnings below.

A musl program never gets one, whatever the flag says: musl reads
`/etc/ld-musl-<arch>.path` and ignores `ld.so.cache` entirely, so a cache would
look like a fix and change nothing. Such a bundle is reported instead.

## Diagnostics

Every message `elfpak` prints carries a stable code: `error[E2001]` when the run
failed and nothing was written, `warning[E2005]` when it succeeded but found
something worth saying. Scripts match on these, so they do not change, and no
code ever means two things — errors and warnings share one namespace, declared
and checked together in `crates/elfpak-core/src/diagnostics.rs`.

The family says what a code is about: `E1xxx` reads an object, `E2xxx` resolves
a dependency, `E3xxx` touches a path, `E4xxx` is configuration, `E5xxx` is
verification.

### Errors

An error ends the run with a non-zero exit status.

| Code    | Meaning |
|---------|---------|
| `E1000` | An I/O operation on a named path failed. |
| `E1001` | A file starts with the ELF magic but does not parse. |
| `E1002` | The input is not an ELF object at all. |
| `E1003` | The target architecture is not one `elfpak` supports. |
| `E1005` | A bounded resource — closure size, graph size, search path — was exceeded. |
| `E1006` | A source file changed between planning and writing. |
| `E2001` | A `DT_NEEDED` library could not be resolved; the directories searched are listed. |
| `E2002` | A library is not on the `--allow-library` list. |
| `E2003` | A candidate was found but targets the wrong architecture. |
| `E2004` | A runtime policy feature found nothing to include (no CA bundle, no tzdata). |
| `E3001` | A path would escape the source or output root. |
| `E3002` | An `--include` names something the source root does not have. |
| `E3003` | Too many symlink hops while resolving a path. |
| `E4001` | Invalid configuration: a bad flag value, a missing output, a colliding install path. |
| `E4002` | A manifest could not be read or does not parse. |
| `E5001` | `elfpak verify` found at least one problem. |

### Warnings

A warning never fails a build. It reports something static analysis found that
the bundle cannot express, and it is recorded in the manifest.

| Code    | Meaning |
|---------|---------|
| `E1004` | An object references `dlopen`, so its runtime closure is not fully knowable. |
| `E2005` | A library was found somewhere the loader will not look inside the bundle. |
| `E2006` | `--install` moves an executable that declares `$ORIGIN`-relative search paths. |
| `E4003` | `--user` was given without `passwd`/`group` files. |

`E2005` and `E2006` describe the two ways a bundle can end up unable to load a
library it contains: one where the library is in a directory the loader does not
search, and one where the executable's own `$ORIGIN`-relative search paths stop
pointing at anything once it is installed elsewhere. Both are fixed rather than
merely reported — they are exactly the conditions under which a loader cache is
generated — so they only appear when `--ld-so-cache=false` rules that out, or
for a target whose cache format `elfpak` cannot write.

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

Supported target architectures are x86_64 and aarch64. Anything else fails with
`E1003`, which names the architecture it found and the raw `e_machine` value.

## Guarantees and determinism

`elfpak bundle`:

* does not execute the target
* does not call `ldd` or `ldconfig` (a loader cache, when one is needed, is
  generated directly from the plan)
* does not execute shell commands
* does not contact the network
* does not invoke Docker
* treats the source filesystem as read-only
* writes only beneath `--output`
* records every included file and why

Output is deterministic for the same application binary, source root,
configuration and `elfpak` version: entries are ordered by destination, file
modes are normalized to `0755`/`0644`, and file *and directory* timestamps are
pinned to `SOURCE_DATE_EPOCH` (default: the Unix epoch), so an image layer built
from the directory output does not change from run to run either. Symlink
timestamps are the one thing that cannot be pinned portably; the tar backend has
no such limitation and is byte-identical.

Path handling is defensive throughout: destinations are normalized lexically,
`..` can never escape the output root, writes through a symlinked parent are
refused, and anything already occupying a destination is unlinked before it is
written — a leftover symlink is never followed out of the output root.
`--clean` refuses to delete a filesystem root.

## Repository layout

```text
crates/elfpak/        CLI: argument parsing, config loading, rendering
crates/elfpak-core/   library: elf, graph, resolver, plan, rootfs, manifest
fixtures/axum-server/ integration fixture: a real Axum service
fixtures/musl-hello/  integration fixture: a musl-linked program
fixtures/vendor-lib/  integration fixture: a library outside the loader's path
fuzz/                 cargo-fuzz targets for the parsing boundary
tests/docker/         Docker smoke tests, one Dockerfile per scenario
Dockerfile            static elfpak distribution image (FROM scratch)
```

`elfpak-core` holds the reusable implementation; no resolution logic lives in
the CLI crate. `crates/elfpak` is itself a library with a six-line `main.rs`
around it: `lib.rs` dispatches, one module per subcommand does the work.

Inside `elfpak-core` the dependencies run one way: `elf` and `source` read the
world, `resolver` turns a soname into a file, `graph` records what was found and
why, `plan` turns that into a `BundlePlan`, and `rootfs` writes one. `plan` is
the only module that decides anything, and nothing downstream of it re-resolves.
`diagnostics` sits beside them all and owns every code the CLI prints.

Base images are pinned by tag and digest, and no build step installs packages
from a distribution mirror, so a digest really does describe what was built.
Every image the tests build is produced for all architectures under test in one
buildx invocation.

## Building the elfpak image

The distribution image is multi-platform and cross-compiled. Its builder stage
runs on the *build* platform (`FROM --platform=$BUILDPLATFORM`) and targets
`$TARGETARCH`, so every architecture is produced by a native compiler and none
of it runs under emulation:

```console
$ docker buildx build --platform linux/amd64,linux/arm64 -t elfpak:local --load .
```

`elfpak` has no C dependencies, so `rust-lld` links every supported target and
no cross toolchain has to be installed. The result is a static musl binary per
architecture, together in one image.

`--load` writes a multi-platform image into the local image store, which
requires the containerd image store to be enabled; `--push` writes it to a
registry and has no such requirement. Where a multi-platform image cannot be
loaded, the smoke tests fall back to one tag per platform.

See the [Docker multi-platform documentation][multi-platform] for the details.

[multi-platform]: https://docs.docker.com/build/building/multi-platform/

## Development

Minimum supported Rust version: **1.97**.

```console
$ just check                       # fmt, clippy -D warnings, and the whole suite
$ just test                        # unit and integration tests
$ tests/docker/smoke.sh            # all Docker smoke tests
$ tests/docker/smoke.sh axum       # Axum on scratch, host architecture
$ tests/docker/smoke.sh axum-arm64 # Axum on scratch, linux/arm64
$ tests/docker/smoke.sh ca         # CA roots come from the bundle, not the binary
$ tests/docker/smoke.sh musl       # a dynamically linked musl program
$ tests/docker/smoke.sh ldcache    # a library the loader only finds through a cache
$ tests/docker/smoke.sh tar        # the same service delivered as a tar and ADDed
$ tests/docker/smoke.sh verify     # `elfpak verify` as a build gate
$ tests/docker/smoke.sh cross      # non-Rust cross-architecture packaging

$ tests/docker/smoke.sh --fresh    # remove the suite's images, build with --no-cache
```

`--fresh` exists so that a rerun cannot be explained by a layer that was already
there: it removes every `elfpak:local*` and `elfpak-*:local*` image and passes
`--no-cache` to each build. BuildKit cache mounts survive it, which keeps cargo
from recompiling the fixtures every time; clear those separately with
`docker builder prune --filter type=exec.cachemount`.

The design is inspired by [TigerStyle](https://tigerstyle.dev/), with safety,
performance, and developer experience prioritized in that order. In practice
that means bounded input-driven work, explicit invariants, deterministic output,
and ordinary Rust formatting and linting. [STYLE.md](STYLE.md) describes the
adaptation. It deliberately avoids mechanical measures such as assertion
density or maximum function length.

The cargo suite covers ELF parsing, token expansion, `ld.so.cache` parsing,
policy evaluation, manifest round-trips and filesystem safety, plus loader
semantics against a synthetic sysroot of purpose-built C fixtures (transitive
`DT_NEEDED`, RPATH inheritance versus RUNPATH non-inheritance, `$ORIGIN`,
`ld.so.conf` globs, cache-only libraries, decoy files, symlinked DSOs).

It also compares against the real glibc loader. Host binaries give breadth:
`elfpak`'s closure must equal what `ldd` reports. Purpose-built fixtures give
depth, because a normal system binary never exercises the interesting rules —
they are installed at absolute paths on the host so the real loader resolves
them too, and then:

* an executable with `DT_RPATH` must resolve its dependency's dependency, in
  both `elfpak` and glibc;
* the same layout with `DT_RUNPATH` must fail in both, since `DT_RUNPATH` is not
  inherited (`ldd` reports `not found`, `elfpak` reports `E2001`);
* `$ORIGIN`-relative search must produce the same closure as the loader.

The generated loader cache is checked against glibc the same way: `ldconfig -p`
must be able to read it, and — where unprivileged user namespaces are available
— the real loader must resolve a library through it, including one bundle that
is `chroot`ed into and executed to prove a rootfs whose library lives outside
every default directory actually starts. Both are skipped rather than failed
where the environment cannot support them.

`ldd` and `ldconfig -p` are test oracles only; the tool itself never runs them.
The interpreter is excluded from the `ldd` comparison, because `ldd` only prints
it when `libc.so.6` declares it as `DT_NEEDED`.

### Fuzzing

The ELF parser is the only place where `elfpak` consumes untrusted binary input.
`tests/elf_robustness.rs` runs deterministic truncation, bit-flip and garbage
mutations on every `cargo test`, and `fuzz/` holds a `cargo-fuzz` target for
deeper runs:

```console
$ cargo install cargo-fuzz
$ mkdir -p fuzz/corpus/parse_elf && cp /usr/bin/ls /usr/bin/true fuzz/corpus/parse_elf/
$ cargo +nightly fuzz run parse_elf
```

The corpus is not checked in; seed it from any system binaries. The fuzz crate is
outside the workspace, so a normal build never needs nightly.

The Docker smoke tests:

* `axum` — builds the Axum fixture, packages it, and asserts that the
  `FROM scratch` image starts, binds an unprivileged port, resolves DNS,
  performs outbound HTTPS, writes to `/tmp`, runs as uid 65532, and contains no
  shell. The HTTPS check uses a plain client with no CA configuration in the
  application at all.
* `ca` — bundles the same binary with `--preset minimal` and asserts that it
  still starts but can no longer make an HTTPS request: the trust roots come
  from the bundle, not from the application.
* `axum-arm64` — the same end-to-end test on `linux/arm64`: the service, the
  `elfpak` that packages it, and the resulting scratch image are all aarch64.
  The full run builds both architectures in a single buildx invocation. The
  application is deliberately *not* cross-compiled, so that the arm64 run
  exercises a natively built binary; on an x86_64 host that compile runs under
  qemu and is the slowest part of the suite.
* `ldcache` — packages a program whose library lives in `/opt/vendor/lib`,
  which the loader never searches and which no `DT_RPATH` names. On the build
  image `ldconfig` makes it work; in the scratch image only the cache `elfpak`
  generated can. The same bundle is then built with `--ld-so-cache=false` and
  must fail to start.
* `musl` — compiles a dynamically linked C program with Alpine's toolchain and
  packages it, on every architecture under test. It compiles inside the pinned
  Rust Alpine image, which already carries that toolchain, so the test installs
  no packages and needs no network of its own. musl-specific behaviour is a non-goal, but
  generic ELF handling has to work: the loader *is* libc
  (`libc.musl-x86_64.so.1` is a symlink to `ld-musl-x86_64.so.1`), there is no
  `ld.so.cache`, and name resolution needs no NSS modules. Each scratch image
  must run and resolve DNS.
* `tar` — the same Axum service as `axum`, delivered as an archive instead of a
  directory. The build stage packages it with `--tar` and asserts in place that
  a second bundle of the same inputs is byte-identical; the archive and its
  manifest are then exported with `--output type=local`, and a second build uses
  that directory as its context so `ADD rootfs.tar /` unpacks it into a
  `FROM scratch` image. Two builds, because `ADD` extracts from the build
  context and there is no `ADD --from` — which is exactly how a pipeline
  consumes an archive. The resulting image then has to pass every assertion the
  `axum` test makes, so an archive-delivered image is held to the same standard
  as a directory-delivered one.
* `verify` — `elfpak verify` as a build gate. One stage bundles, the next
  verifies against the manifest, and the image is copied out of the stage that
  verified: a failure fails the build, so the rootfs that ships is the rootfs
  that was checked. The middle stage covers the negative space too — changed
  bytes, changed permissions, an added file, a removed file, a redirected
  symlink — and distinguishes what `--strict` catches from what plain
  verification catches. The suite then rebuilds the same Dockerfile with
  `--build-arg ELFPAK_TAMPER=1`, which corrupts the rootfs before verification,
  and requires that build to fail: a gate that cannot fail proves nothing.
* `cross` — exports an aarch64 sysroot, packages an aarch64 binary with the host
  `elfpak`, and runs the result under emulation. Nothing is compiled or executed
  for the foreign architecture, which keeps it cheap and covers the non-Rust
  path.

## Status

Implemented (roadmap 0.1/0.2, plus parts of 0.3 and 1.0): workspace, CLI, ELF
parsing, architecture detection, full static closure,
RPATH/RUNPATH/token/cache/conf resolution, path and symlink preservation,
presets, manifest with recorded policy, hashes, dependency allow-list, `verify`
including strict mode, deterministic directory and tar output, loader-oracle
tests, parser fuzzing, and x86_64 + aarch64 support.

Not implemented yet, by design: OCI output, runtime tracing (`elfpak trace`),
SBOM generation, and musl-specific behaviour beyond generic ELF parsing — which
includes `/etc/ld-musl-<arch>.path`, the musl counterpart of the generated
loader cache.
