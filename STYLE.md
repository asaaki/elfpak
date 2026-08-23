# Coding style

`elfpak` takes inspiration from [TigerStyle](https://tigerstyle.dev/), adapted to an ordinary Rust codebase. The priorities are, in order: **safety, performance, developer experience**. This is design guidance, not a claim of strict TigerStyle compliance.

## Safety

Distinguish bad input and environmental failures from programmer errors. Return [`Error`](crates/elfpak-core/src/error.rs) for the former; use assertions for internal invariants whose failure means `elfpak` is wrong. Assertions are most useful at boundaries, such as when a graph node is accepted or a plan is materialized. Avoid assertions that merely restate the type system or the line immediately above them.

Bound work derived from ELF files and filesystem contents. Use named limits when an input could otherwise grow memory or work without a practical bound, and document why the chosen limit is reasonable. Prefer explicit stacks for input-driven tree and graph walks so their depth is visible and controllable.

Use fixed-width integers for serialized values, file formats, stable IDs, and counters with a meaningful range. Use `usize` for in-memory indexing. Make potentially narrowing conversions explicit.

Treat the source root as read-only. Logical paths belong to the target filesystem; join them to the source or output root before host filesystem operations. Never follow an existing output symlink while writing.

## Performance

Do work once when practical. Parse each ELF object once, cache file digests within a run, stream large files, and build directory and archive output from the same plan. Keep allocations and copies visible; optimize measured hot paths without obscuring the loader semantics.

Determinism is part of performance engineering here: stable ordering and normalized metadata make builds cacheable and failures reproducible.

## Developer experience

Use `rustfmt` and keep Clippy warning-free. Prefer direct control flow and small interfaces. Break up a function when it has more than one responsibility, not to satisfy a line-count rule. A helper should clarify a concept, not merely rename a short expression.

Name values for what they represent. Add units where they prevent ambiguity (`timeout_ms`, `size_bytes`), and preserve the stable diagnostic codes exposed by the CLI. Errors should include the path, library, or option needed to act on them.

Keep dependencies few and well understood. New dependencies should earn their maintenance, compile-time, and supply-chain cost.

## Comments and documentation

Document public behavior and non-obvious decisions. Comments should explain a constraint, compatibility detail, or trade-off; delete narration that repeats the next line of code. Keep the README concise and put command details in `DOCUMENTATION.md`.

## Testing

Test invariants at the boundary where they matter. Add regression tests for bugs and focused coverage for loader behavior, determinism, and filesystem safety. Tests may skip platform features they cannot use, but should make the reason visible.
