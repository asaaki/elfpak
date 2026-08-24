# Coding style

`elfpak` takes ideas from [TigerStyle](https://tigerstyle.dev/) and adapts them to an ordinary Rust codebase. The priorities, in order, are **safety, performance, developer experience**. This page gives design guidance. It does not claim strict TigerStyle compliance.

## Safety

Keep bad input and environmental failures separate from programmer errors. For bad input and environmental failures, return an [`Error`](crates/elfpak-core/src/error.rs). For internal invariants whose failure means `elfpak` is wrong, use an assertion. Assertions work best at boundaries, for example when a graph node is accepted or a plan is materialized. Do not write an assertion that only restates the type system or the line before it.

Bound the work that comes from ELF files and filesystem contents. When an input could grow memory or work without a practical limit, use a named limit and document why the limit is reasonable. For input-driven tree and graph walks, prefer an explicit stack. This keeps the depth visible and controllable.

Use fixed-width integers for serialized values, file formats, stable IDs, and counters with a meaningful range. Use `usize` for in-memory indexing. Make a conversion explicit when it can narrow the value.

Treat the source root as read-only. Logical paths belong to the target filesystem. Join a logical path to the source or output root before you use it in a host filesystem operation. Never follow an existing output symlink while you write.

## Performance

Do a piece of work once when practical: parse each ELF object once, cache file digests within a run, stream large files, and build directory and archive output from the same plan. Keep allocations and copies visible. Optimize a measured hot path without hiding the loader semantics.

Determinism is part of performance work here. Stable ordering and normalized metadata make builds cacheable and failures reproducible.

## Developer experience

Use `rustfmt` and keep Clippy free of warnings. Prefer direct control flow and small interfaces. Break up a function when it has more than one responsibility, not to satisfy a line-count rule. A helper must clarify a concept. It must not just rename a short expression.

Name each value for what it represents. Add units where they remove ambiguity (`timeout_ms`, `size_bytes`). Keep the stable diagnostic codes that the CLI exposes. An error message must include the path, library, or option that a reader needs to act on it.

Keep the number of dependencies low, and use only dependencies you understand well. A new dependency must earn its maintenance, compile-time, and supply-chain cost.

## Comments and documentation

Document public behavior and decisions that are not obvious. A comment must explain a constraint, a compatibility detail, or a trade-off. Delete a comment that only repeats the next line of code. Keep the README short. Put command details in `DOCUMENTATION.md`.

## Testing

Test an invariant at the boundary where it matters. Add a regression test for each bug. Add focused test coverage for loader behavior, determinism, and filesystem safety. A test can skip a platform feature it cannot use, but it must show the reason.
