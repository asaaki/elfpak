# Multi-Executable Bundles Design

## Goal

Allow one elfpak invocation to package multiple executables into one atomic rootfs, tar archive,
and manifest. Add Cargo-aware selectors for every workspace binary, every binary in one package,
or a named subset of one package.

## User interface

`elfpak bundle` accepts one or more positional executable paths. A singular invocation keeps the
existing `--install <PATH>` behavior. `--install-dir <DIR>` installs each input under its original
basename and works for singular and plural invocations. `--install` and `--install-dir` conflict,
and `--install` is rejected when more than one executable is selected. With neither option, each
executable is installed at `/<basename>`, preserving the existing default for one input.

The optional configuration file gains `package.binaries` and `package.install_dir`. The singular
`package.binary` and `package.install` keys remain compatible. CLI positional inputs override both
configured binary forms. Conflicting singular/plural configuration or duplicate install paths are
reported before planning or output writes.

`cargo elfpak bundle` retains existing default inference and `--bin <NAME>`, and adds:

- `--all`: all binary targets in every workspace member; library-only packages are skipped.
- `--all-bins`: all binary targets in the selected or inferred package.
- `--bins <NAME>,<NAME>`: a comma-delimited subset in the selected or inferred package.

`--all` conflicts with package and binary selectors. `--bin`, `--bins`, and `--all-bins` are
mutually exclusive. Selections and emitted summaries are sorted by package and binary name.
Unknown names list the valid choices. Two selected targets that would have the same installed
basename are rejected with the packages that caused the collision.

## Core planning model

The core planner owns all selected `(source binary, install path)` pairs and resolves every runtime
closure before writing. Each closure remains a separate dependency graph so dependency reasons,
interpreter information, and per-application diagnostics stay accurate. The resulting `BundlePlan`
contains an ordered collection of application plans plus one deduplicated, destination-sorted file
set shared by every materializer.

All executable architectures must match. Shared closure entries may overlap only when their kind,
contents, and symlink targets agree. Executable destinations must be unique and may not displace a
library required by another application. These checks happen before runtime policy or output
materialization.

Loader-cache planning is bundle-wide. If any closure needs an `ld.so.cache`, the generated cache
contains the compatible interpreter and shared-object entries from every glibc closure. This avoids
the incomplete-cache failure that would result from merging independently finished plans. Runtime
policy and explicit includes are applied once to the shared plan.

Existing single-application APIs remain source compatible: `Planner::new`, `BundlePlan::executable`,
`BundlePlan::graph`, and `elfpak::run_bundle` continue to address the first/only application. New
plural accessors and `elfpak::run_bundle_many` expose multi-application behavior.

## Manifest and rendering

Manifest version 3 adds a `binaries` array containing every installed executable. The existing
`binary` field remains the primary executable for compatibility, and old manifests deserialize with
an empty `binaries` array that semantically means the singular `binary` value.

Bundle summaries print one `source -> destination` mapping per executable, followed by the shared
entry counts, destinations, and warnings. Directory, tar, manifest, dry-run, clean, verification,
and quiet behavior remain one operation over the combined plan.

## Cargo build flow

Metadata selection returns an ordered set of `(PackageId, package name, binary name)` values and a
build scope. Cargo is invoked once:

- `--workspace --bins` for `--all`;
- `--package <PACKAGE> --bins` for `--all-bins`; or
- one `--package` plus repeated `--bin` selectors for singular and subset modes.

JSON compiler-artifact messages are matched by package ID and binary target name. Every selection
must produce exactly one executable; results are restored to deterministic selection order before
being passed to `run_bundle_many`. Existing profile, target, feature, lock, offline, frozen, quiet,
and freshness behavior applies to the whole build.

## Errors and output safety

Selector, destination, architecture, collision, missing-artifact, and dependency-policy failures
occur before any rootfs, tar, or manifest is published. Output staging remains unchanged and atomic.
No executable is renamed in multi-binary mode; users resolve basename collisions by selecting a
subset or building/package-naming distinct binaries.

## Verification

Focused tests cover plural path resolution, cross-application closure deduplication and collisions,
manifest compatibility, all Cargo selector modes, selector conflicts, missing targets, build artifact
collection, and end-to-end dry-run/materialized bundles. The final gate is `just check`, followed by
literal CLI smoke invocations for standalone multi-input and Cargo workspace `--all` behavior.
