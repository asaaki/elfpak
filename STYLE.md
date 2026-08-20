# Coding style

`elfpak` follows [TigerStyle](https://tigerstyle.dev/)
([TIGER_STYLE.md](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md)),
adapted to Rust. The design goals, in this order, are **safety, performance,
developer experience**. This file records what that means here, so that a
reviewer can check the code against something written down.

## Safety

**Assertions detect programmer errors.** Operating errors — a missing library,
an unreadable file, a malformed `ld.so.cache` — are expected, and every one of
them is an [`Error`](crates/elfpak-core/src/error.rs) variant with a stable
diagnostic code. An assertion failure is different: it means `elfpak` itself is
wrong, and the only correct response is to stop. Release builds keep assertions
on (`assert!`, not `debug_assert!`) and `panic = "abort"`, so a corrupt run
cannot go on to write a rootfs.

Assertions state pre- and postconditions, not decoration:

* every logical path is absolute, because it names a location in the *target*
  filesystem rather than on this host;
* every digest that enters the graph is a well-formed SHA-256;
* `join_under` asserts containment, which is the entire reason it exists;
* a plan entry is checked when it enters the plan and again before it is
  written — a [pair assertion](https://tigerbeetle.com/blog/2023-12-27-it-takes-two-to-contract)
  on both sides of the plan;
* a generated `/etc/ld.so.cache` is parsed back with the same reader the loader
  is modelled on before it is accepted.

Compound assertions are split (`assert!(a); assert!(b);`) so a failure says
which half failed. Compile-time relationships between constants are asserted
with `const _: () = assert!(…)`.

**Put a limit on everything.** Every loop is bounded, and the bound is a named
constant with a comment saying why that number: `NODES_MAX`, `EDGES_MAX`,
`SYMLINK_HOPS_MAX`, `PENDING_COMPONENTS_MAX`, `SEARCH_DIRECTORIES_MAX`,
`CONF_DEPTH_MAX`, `CONF_FILES_MAX`, `CACHE_ENTRIES_MAX`. Where a walk is bounded
by the size of a structure rather than by a constant, the loop asserts it.

**No recursion.** `ld.so.conf` includes, the `DT_NEEDED` closure, directory
trees and rootfs walks are all explicit stacks, so the depth of a walk is a
number in the function rather than a property of the machine's stack.

**Explicitly-sized types.** `NodeId` is a `u32`, and report counters are `u32`
or `u64`. `usize` is used only where it is an index into memory this process
owns. Casts that could truncate are rejected by lint; where a conversion is
intentional it is spelled `try_from(…)` with the reason.

**All errors are handled.** No `unwrap()` on anything that depends on the
environment. `expect()` is used only for conditions an assertion has already
established, and its message says which one.

## Structure

* **A hard limit of 70 lines per function, and 100 columns per line.** Both are
  enforced by [`crates/elfpak-core/tests/style.rs`](crates/elfpak-core/tests/style.rs),
  so they are part of `cargo test` rather than part of code review.
* **Centralize control flow.** `Planner::plan`, `Resolver::resolve`,
  `RootFsBuilder::apply` and `Manifest::verify` hold the branches of their
  operation; the helpers around them compute one thing each and take no
  decisions. Push `if`s up and `for`s down.
* **Order matters.** A file reads top-down: types, then the entry point, then
  the helpers it calls, in call order.
* **One block of imports per file**, sorted, with each crate merged into a
  single `use`. `just imports` reflows them; the two rustfmt options that do it
  are nightly-only, so they live in the recipe rather than in `rustfmt.toml`.
* **Names carry units and qualifiers last**, most significant word first, so
  related names line up: `MANIFEST_NAME_DEFAULT`, `CONFIG_NAME_DEFAULT`,
  `SYMLINK_HOPS_MAX`, `HASH_BUFFER_SIZE_BYTES`.

## Comments

Comments say **why**, in whole sentences. Restating the code is not a comment.
Where a decision is not obvious — why musl gets no `ld.so.cache`, why an
`ld.so.conf` include is read once, why the cache table is sorted downwards — the
reason is in the code, next to the decision.

## Dependencies and tooling

The dependency list is short and boring, and stays that way: `goblin` for ELF
parsing, `sha2`, `tar`, `serde`/`serde_json`/`toml`, `clap`, `thiserror`,
`anyhow`. No build scripts, no proc-macro-heavy frameworks, no network at
runtime.

One toolbox, driven by `just`:

```console
$ just check   # fmt-check + clippy -D warnings + the whole test suite
$ just style   # the numeric limits on their own
$ just imports # reflow the use blocks (needs nightly rustfmt)
$ just smoke   # Docker smoke tests
```

## Deviations, and why

* **Static allocation.** TigerBeetle allocates everything up front. `elfpak`
  cannot: the size of a closure is discovered by reading the binary. Instead,
  every structure that grows has an asserted upper bound, which is what the
  rule is really after.
* **`snake_case` file and function names** are Rust's convention already; type
  names stay `CamelCase` rather than following Zig.
* **Assertion density.** TigerStyle asks for an average of two assertions per
  function. Trivial accessors and `as_str` conversions here have none; the
  functions that resolve, plan, and write carry the density instead.
