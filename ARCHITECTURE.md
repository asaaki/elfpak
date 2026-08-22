# Architecture

`elfpak` packages a Linux ELF executable and its runtime closure into a small,
deterministic root filesystem suitable for `FROM scratch` images. It is a Rust
workspace with an adapter-focused command-line crate over a reusable core
library.

## System at a glance

```mermaid
flowchart TB
    Inputs["CLI arguments<br/>optional elfpak.toml"] --> Adapter["crates/elfpak<br/>application adapter"]

    subgraph Core["crates/elfpak-core"]
        Source["SourceRoot<br/>logical target filesystem"] --> Elf["ELF parser"]
        Elf --> Resolver["dynamic-linker resolver"]
        Resolver --> Graph["DependencyGraph"]
        Policy["runtime + dependency policy"] -->|"may add NSS modules"| Graph
        Graph --> Planner["Planner"]
        Policy --> Planner
        Planner --> Plan["BundlePlan<br/>read-only public view"]
    end

    Adapter --> Source
    Adapter --> Policy
    Plan --> Inspect["inspect / dry-run"]
    Plan --> Rootfs["rootfs directory"]
    Plan --> Tar["deterministic tar"]
    Plan --> Manifest["manifest"]
    Rootfs --> Verify["verify"]
    Manifest --> Verify
```

The `BundlePlan` is the central boundary: discovery and validation finish
before any output is written. Its fields are crate-private and external callers
receive read-only views, so the public API preserves planner-established
invariants. Both directory and tar output consume the same plan; neither
re-resolves dependencies.

## Workspace and ownership

| Location | Responsibility |
|---|---|
| `crates/elfpak` | Application boundary: Clap argument definitions, `elfpak.toml` parsing and precedence, command dispatch, output-path validation, and terminal rendering. |
| `crates/elfpak-core` | Reusable domain library: ELF parsing, source-root abstraction, loader-faithful dependency resolution, policy, graphing, plan construction, materialization, archives, manifests, and verification. |
| `fixtures/` | Small real programs used by integration and Docker tests: Axum, musl, and an off-path vendor library. |
| `crates/elfpak-core/tests/` | Unit/integration, filesystem-safety, resolver, robustness, and glibc-oracle tests. |
| `tests/docker/` | End-to-end scratch-image smoke scenarios. |
| `fuzz/` | Separate nightly `cargo-fuzz` harness for the ELF parser boundary. |

`crates/elfpak/src/main.rs` is only the executable entry point. Its library
dispatches `inspect`, `bundle`, and `verify`; it deliberately contains no ELF
or resolver logic. Workspace-wide lint policy forbids `unsafe` and the release
profile favors a small static utility (`opt-level = "z"`, LTO, stripped,
`panic = "abort"`).

## Core model and data flow

### 1. Input and source filesystem

`SourceRoot` treats `--root` as the target system's logical `/`. It maps
logical paths to host paths while resolving symlinks inside that root only.
Symlink traversal and pending components are bounded, which prevents both
host-root escapes and unbounded path processing. This is what enables
cross-architecture packaging without executing the target.

The optional `elfpak.toml` is parsed with unknown fields rejected. For
`bundle`, command-line arguments override config values, which override
defaults. Paths that describe the project are rebased to the config file;
logical in-image paths remain absolute target paths.

### 2. ELF analysis and dependency resolution

`elf` uses `goblin` to parse ELF metadata: architecture/class/endianness,
object type, `PT_INTERP`, `DT_NEEDED`, RPATH/RUNPATH, loader flags, SONAME, and
references to `dlopen`-style APIs. Only x86_64 and aarch64 targets are accepted.

`Resolver` constructs the runtime closure without running the executable,
`ldd`, or `ldconfig`. It models the relevant glibc lookup inputs:

- the interpreter and recursive `DT_NEEDED` edges;
- RPATH inheritance and non-inherited RUNPATH;
- `$ORIGIN`, `$LIB`, and `$PLATFORM` expansion;
- explicit `--library-path` directories;
- parsed `ld.so.cache` and `ld.so.conf` (including include globs);
- architecture-specific default directories, while deliberately excluding
  CPU-specific glibc-hwcaps variants that the target filesystem alone cannot
  prove are safe to run; and
- `DF_1_NODEFLIB` plus candidate ABI validation.

Resolution builds a bounded `DependencyGraph`. Each node retains its original
logical destination, source file, symlink chain, digest, selected ELF metadata,
and inclusion relationship. The graph distinguishes application dependencies,
the kernel-loaded interpreter, and policy-added dependencies.

### 3. Planning and policy

`Planner` is the decision point. It converts the graph to a sorted
`BundlePlan`, after checking the install path, dependency allow-list, and any
install-path collision with a required library. Planned entries are explicit
directories, symlinks, source-backed regular files, or generated files. Each
has a destination, normalized mode, size and digest where applicable, kind,
and inclusion reason.

Planning is one architectural stage but is split internally by role:
`plan/model.rs` defines the read-only plan data, `plan/builder.rs` owns entry
construction and destination precedence, and `plan/mod.rs` orchestrates the
resolver and policy decisions.

Runtime policy is separate from statically discoverable ELF dependencies:

- `minimal` contains only the executable closure;
- `web` additionally requests CA roots, `/tmp`, generated passwd/group,
  generated `nsswitch.conf`, and available NSS modules;
- timezone data and explicit includes are opt-in;
- `--user` supplies generated identity data when passwd/group is enabled.

The planner also decides whether to generate `/etc/ld.so.cache`. It does so
when a resolved library was found through a location the packaged loader would
not otherwise search, or when relocating an `$ORIGIN`-dependent executable
would break lookup. The cache is built directly from the planned closure. It
is intentionally not generated for musl, which does not use this cache.

Static analysis cannot determine `dlopen` dependencies. The plan remains valid
but carries a stable warning; callers can add known runtime files with
`--include`.

### 4. Materialization and attestation

`RootFsBuilder` writes a directory plan through a sibling staging directory,
then publishes it. It rejects unsafe output roots and refuses writes through
symlinked parents. Source-backed entries are rehashed while being copied, so a
source change after planning fails rather than producing an unrecorded artifact.
Without `--clean`, pre-existing unplanned output files are preserved; with it,
the finished staged tree replaces the old rootfs.

`TarBuilder` writes directly from the plan, not from the directory output. Tar
paths are relative; ordering, ownership, modes, and timestamps are pinned.
Directory output similarly normalizes modes and pins file/directory timestamps
using `SOURCE_DATE_EPOCH` (default 0).

`Manifest` records the plan, resolved policy, output locations, warnings, and
every planned entry's path, kind, reason, mode, size, digest, or link target.
It is written beside the artifacts, never into a rootfs. `verify` validates the
manifest before checking a materialized tree; normal verification detects
missing or altered entries, while `--strict` additionally detects unlisted
entries and mode changes.

## Operational invariants

- The source filesystem is read-only from elfpak's perspective; output is
  planned before materialization.
- Original library paths and symlink topology are preserved rather than
  relocated behind `LD_LIBRARY_PATH`.
- Output entries are deterministic for equal inputs and tool version. Tar
  output is byte-stable; directory symlink timestamps are the documented
  platform limitation.
- Bounded graph, cache, search-path, directory-walk, and symlink processing
  protect against malformed or unexpectedly large inputs.
- Stable diagnostic codes are centralized in `diagnostics.rs` and shared by
  errors and warnings.

## Verification and delivery pipeline

`just check` runs formatting verification, Clippy with warnings denied, and the
workspace test suite. The suite covers parser robustness, path safety,
determinism, plan/manifest behavior, resolver semantics, and comparisons with
the real glibc loader. Docker smoke tests add scratch-image scenarios for web
services, CA roots, musl, generated loader caches, tar delivery, verification,
and cross-architecture packaging.

The top-level `Dockerfile` builds a static musl `elfpak` for amd64 or arm64
with `rust-lld`, then publishes it in `FROM scratch`. The build stage runs on
the build platform and cross-compiles the requested target, avoiding emulation
for the tool itself.

## Scope and current design assessment

The architecture is internally consistent: dependency discovery, top-level
policy, planning, output, and verification have clear one-way boundaries
centered on the plan. CLI file configuration remains outside the reusable core,
and the test strategy independently checks the most loader-sensitive behavior.

Deliberate limitations are documented scope rather than design defects:
only x86_64/aarch64 are supported; `dlopen` cannot be fully discovered
statically; OCI output, runtime tracing, and SBOM generation are not yet
implemented.
