# Multi-Executable Bundles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Package multiple standalone or Cargo-built executables into one atomic elfpak bundle.

**Architecture:** Extend the core planner from one root closure to an ordered collection of application closures that share one file plan and loader cache. Keep singular public APIs as first-application compatibility shims, then teach the standalone and Cargo adapters to resolve plural inputs and selections before one materialization.

**Tech Stack:** Rust 1.98, Clap 4, cargo_metadata 0.23, serde/serde_json, existing elfpak planner and integration-test harnesses.

**Spec:** `docs/superpowers/specs/2026-08-22-multi-executable-bundles-design.md`

## Global Constraints

- All selected executables in one bundle must have the same architecture.
- Multi-input install names are preserved; duplicate destinations fail before outputs are written.
- Directory, tar, and manifest outputs describe the same combined plan and remain atomic.
- Existing singular CLI invocations, `Planner::new`, and `elfpak::run_bundle` remain compatible.
- No new third-party dependencies are introduced.
- All selection and plan ordering is deterministic.

---

### Task 1: Multi-application core plan model

**Files:**
- Modify: `crates/elfpak-core/src/plan/model.rs`
- Modify: `crates/elfpak-core/src/plan/mod.rs`
- Modify: `crates/elfpak-core/tests/rootfs.rs`

**Interfaces:**
- Produces: `Planner::add_binary(binary: impl Into<PathBuf>, install_path: impl Into<PathBuf>) -> Planner`
- Produces: `ApplicationPlan::{executable, graph, interpreter, interpreter_resolved}` read-only accessors
- Produces: `BundlePlan::applications() -> &[ApplicationPlan]` and `BundlePlan::executables()` iterator
- Preserves: singular accessors return the first application

- [ ] **Step 1: Write failing combined-plan tests**

Add tests that plan two real fixture executables, assert two executable destinations exist in one
`BundlePlan`, assert common shared objects occur once in `files()`, and assert duplicate executable
destinations return a configuration error. The observable mutation each test catches is a planner
that drops the second root or silently overwrites a destination.

```rust
let plan = Planner::new(SourceRoot::new(&sysroot.root), sysroot.path("/bin/first"))
    .install_as("/app/first")
    .add_binary(sysroot.path("/bin/second"), "/app/second")
    .plan()
    .unwrap();
assert_eq!(plan.applications().len(), 2);
assert_eq!(plan.executables().count(), 2);
```

- [ ] **Step 2: Verify the focused tests fail**

Run: `cargo test -p elfpak-core --test rootfs multi`

Expected: compilation failure because plural planner/model APIs do not exist.

- [ ] **Step 3: Implement application collection and collision validation**

Introduce a private planner input pair and public `ApplicationPlan`. Resolve every input into its own
graph, require matching architecture, validate dependency policy per graph, and validate node and
symlink destinations across graphs before feeding them to one `PlanBuilder`. Apply runtime policy
once. Preserve existing singular methods by indexing the non-empty first application.

- [ ] **Step 4: Build one loader cache from every closure**

Aggregate unreachable/relocated decisions across `(graph, resolver)` pairs. When the runtime policy
enables a cache, collect interpreter/shared-object cache entries from every glibc graph and call the
existing cache builder once. Deduplicate warnings and cache entries deterministically.

- [ ] **Step 5: Verify core tests pass**

Run: `cargo test -p elfpak-core --test rootfs multi && cargo test -p elfpak-core`

Expected: all core unit and integration tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/elfpak-core/src/plan crates/elfpak-core/tests/rootfs.rs
git commit -m "feat: plan multiple executable closures"
```

### Task 2: Standalone plural input and manifest behavior

**Files:**
- Modify: `crates/elfpak/src/cli.rs`
- Modify: `crates/elfpak/src/config.rs`
- Modify: `crates/elfpak/src/bundle/paths.rs`
- Modify: `crates/elfpak/src/bundle/mod.rs`
- Modify: `crates/elfpak/src/render.rs`
- Modify: `crates/elfpak/src/lib.rs`
- Modify: `crates/elfpak-core/src/manifest.rs`
- Test: `crates/elfpak/tests/cli.rs`
- Test: manifest tests in `crates/elfpak-core/tests/rootfs.rs`

**Interfaces:**
- Produces: positional `Vec<PathBuf>` for `elfpak bundle`
- Produces: `BundleArgs::install_dir: Option<PathBuf>`
- Produces: `elfpak::run_bundle_many(args, binaries, quiet, verbose)`
- Produces: manifest v3 `binaries: Vec<String>` while retaining `binary`

- [ ] **Step 1: Write failing standalone CLI tests**

Use two distinct host ELF executables. Materialize them with
`elfpak bundle <first> <second> --install-dir /app --output <rootfs> --no-config`; assert both
basename paths exist, the manifest lists both, and verification succeeds. Add failures for plural
`--install`, duplicate basenames, and `--install` plus `--install-dir`.

- [ ] **Step 2: Verify the CLI test fails**

Run: `cargo test -p elfpak --test cli multiple_binaries`

Expected: Clap rejects the second positional executable or the new option.

- [ ] **Step 3: Implement plural path resolution**

Represent resolved inputs as:

```rust
pub(crate) struct BundleInput {
    pub(crate) binary: PathBuf,
    pub(crate) install: PathBuf,
}
```

CLI positional inputs override `package.binaries`/`package.binary`. Singular `--install` is allowed
only for one input; otherwise join each basename beneath `--install-dir` or `/`. Reject empty names,
conflicting config forms, and duplicate normalized destinations.

- [ ] **Step 4: Dispatch and render one combined plan**

Construct the planner from the first resolved input, append the rest, write outputs once, and print
one source/destination line per `ApplicationPlan`. Keep `run_bundle` as a wrapper around
`run_bundle_many` with one path.

- [ ] **Step 5: Add manifest v3 plural representation**

Serialize all executable destinations in `binaries`, retain the first in `binary`, default
`binaries` when reading v1/v2 manifests, and validate each path. Add a round-trip test proving both
applications survive serialization and an old-manifest test proving singular compatibility.

- [ ] **Step 6: Verify standalone and core tests pass**

Run: `cargo test -p elfpak --test cli && cargo test -p elfpak-core`

Expected: all tests pass, including singular compatibility.

- [ ] **Step 7: Commit**

```bash
git add crates/elfpak crates/elfpak-core/src/manifest.rs crates/elfpak-core/tests/rootfs.rs
git commit -m "feat: bundle multiple executables"
```

### Task 3: Cargo plural metadata selection

**Files:**
- Modify: `crates/cargo-elfpak/src/cli.rs`
- Modify: `crates/cargo-elfpak/src/metadata.rs`

**Interfaces:**
- Produces: `SelectionSet { binaries: Vec<Selection>, build_scope: BuildScope }`
- Produces: `BuildScope::{WorkspaceAllBins, PackageAllBins, Selected}`
- Consumes: CLI `--all`, `--all-bins`, `--bins`, existing `--bin` and `--package`

- [ ] **Step 1: Add failing selector unit tests**

Against the existing two-package metadata fixture, assert `--all` returns `api/migrate`,
`api/serve`, and `worker/worker`; `-p api --all-bins` returns both API binaries; and
`-p api --bins migrate,serve` returns the named subset. Add unknown, duplicate-name, empty-workspace,
and selector-conflict coverage. Each expectation uses literal ordered package/binary pairs.

- [ ] **Step 2: Verify selector tests fail**

Run: `cargo test -p cargo-elfpak metadata::tests`

Expected: new context fields/types and plural selector behavior are absent.

- [ ] **Step 3: Implement deterministic selection**

Add Clap conflict declarations, parse comma-delimited `--bins`, and return a sorted selection set.
Workspace-all skips packages without binary targets; package-all errors when its package has none.
Explicit subsets validate every name and collapse no duplicates. Reject cross-package binary-name
collisions with an error naming each package.

- [ ] **Step 4: Verify selector tests pass**

Run: `cargo test -p cargo-elfpak metadata::tests`

Expected: all metadata selector tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/cargo-elfpak/src/cli.rs crates/cargo-elfpak/src/metadata.rs
git commit -m "feat: select multiple Cargo binaries"
```

### Task 4: Cargo multi-artifact build and dispatch

**Files:**
- Modify: `crates/cargo-elfpak/src/build.rs`
- Modify: `crates/cargo-elfpak/src/lib.rs`
- Test: `crates/cargo-elfpak/tests/cli.rs`

**Interfaces:**
- Consumes: `SelectionSet` and its `BuildScope`
- Produces: `build::run(&BuildRequest) -> Result<Vec<BuildArtifact>>` in selection order
- Consumes: `elfpak::run_bundle_many`

- [ ] **Step 1: Write failing end-to-end tests**

Create temporary real Cargo projects and assert:

```text
cargo elfpak bundle --all --install-dir /app --output rootfs --dry-run --no-config
cargo elfpak bundle -p tools --all-bins --install-dir /app --output rootfs --dry-run --no-config
cargo elfpak bundle -p tools --bins first,third --install-dir /app --output rootfs --dry-run --no-config
```

The output must name exactly the selected artifacts and mappings. Add one materialized subset case
that asserts both installed files exist, plus conflict cases that prove Cargo is not invoked.

- [ ] **Step 2: Verify the end-to-end tests fail**

Run: `cargo test -p cargo-elfpak --test cli multiple_binaries`

Expected: the new selectors are rejected or only one artifact is dispatched.

- [ ] **Step 3: Build the selected scope once**

Emit `--workspace --bins`, `--package P --bins`, or repeated `--bin` arguments according to
`BuildScope`. Match compiler artifacts by both package ID and target name, reject missing or duplicate
matches, and return artifacts in `SelectionSet` order.

- [ ] **Step 4: Dispatch all artifacts to elfpak**

Print one `fresh Cargo binary:` or `built Cargo binary:` line per artifact unless quiet, collect their
paths, and call `run_bundle_many`. Preserve every existing Cargo/build and elfpak bundle option.

- [ ] **Step 5: Verify Cargo adapter tests pass**

Run: `cargo test -p cargo-elfpak`

Expected: unit and end-to-end tests pass for singular and plural modes.

- [ ] **Step 6: Commit**

```bash
git add crates/cargo-elfpak
git commit -m "feat: build and bundle multiple Cargo binaries"
```

### Task 5: Documentation and completion audit

**Files:**
- Modify: `README.md`
- Modify: `DOCUMENTATION.md`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Documents: standalone plural inputs and install rules
- Documents: Cargo selector matrix and collision behavior
- Documents: multi-root core planning and manifest v3

- [ ] **Step 1: Update user documentation**

Add concise examples for `elfpak bundle bin/server bin/migrate --install-dir /app` and each Cargo
selector. State conflicts, defaults, config keys, unchanged basenames, and duplicate-name errors.

- [ ] **Step 2: Update architecture documentation**

Describe multiple closure graphs feeding one shared plan/cache/materialization and update singular
Cargo flow language.

- [ ] **Step 3: Run formatting and focused CLI help checks**

Run:

```bash
cargo fmt --all
cargo run -q -p elfpak -- bundle --help
cargo run -q -p cargo-elfpak -- bundle --help
```

Expected: both help outputs expose the documented selectors and install options.

- [ ] **Step 4: Audit every spec requirement**

Confirm source and test evidence for plural standalone input, `--install-dir`, `--all`,
`-p --all-bins`, `-p --bins`, singular compatibility, collision errors, combined manifest/tar/rootfs,
Cargo build options, documentation, and atomic pre-write validation.

- [ ] **Step 5: Run the full gate and literal smoke tests**

Run `just check`, then invoke a standalone two-host-binary dry run and a temporary two-package Cargo
workspace `--all` dry run. Expected: zero failures and both executable mappings in each smoke output.

- [ ] **Step 6: Commit**

```bash
git add README.md DOCUMENTATION.md ARCHITECTURE.md
git commit -m "docs: explain multi-executable bundles"
```
