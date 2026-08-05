# Task 01 — Establish the Rust foundation and internal contract

## Prompt

You are implementing the first chunk of `jj-conflict-untangler`, a Rust CLI that transports JJ conflict files into agent-editable temporary workspaces. Work only on this task. Do not implement snapshot parsing, `prepare`, or `apply` behavior yet.

## Context and non-negotiable product contract

The tool is for coding agents, not interactive human merge UX. It must make conflict resolution mechanically safe: agents read complete numbered term files and edit one ordinary `resolved` file. The tool is a conflict materializer and guarded installer, not a merge engine.

The v1 design is:

- `prepare --file FILE [--output-dir DIR]` reads a conflicted source and creates an OS temp workspace without modifying the repository.
- `apply --resolved-file FILE [--manifest FILE] [--write]` validates a proposed resolution, prints a diff by default, and modifies the source only with explicit `--write`.
- Temp workspaces use the platform temp directory through secure creation, with a `jj-conflict-<random>`-style name and restrictive permissions. Do not hardcode `/tmp`.
- Every conflict region has `term-000`, `term-001`, ... in materialized order. Terms are complete snapshots supplied by JJ’s snapshot conflict-marker style; names must not assume `base`, `ours`, or `theirs`.
- `resolved` is initially seeded by replacing each region with that region’s first logical term (`term-000`); this is only a structural seed, not a semantic merge.
- `source` retains the original bytes for diagnostics. `manifest.json` records enough metadata to validate later application.
- v1 supports only snapshot-style markers: an opening `<` marker, same-width `+` side / `-` base section headers, and a same-width `>` closing marker. Marker width is inferred and must be at least seven. Structural `%`, backslash, `|||||||`, and `=======` formats are rejected as unsupported.
- All file content is bytes. Preserve LF, CRLF, mixed/no-EOL behavior exactly; never use text/EOL normalization.
- No `jj-lib` dependency in v1. The installed `jj` CLI is the source of truth for materialization style; the helper owns parsing, artifacts, validation, and guarded installation. Ask the operator before adding any dependency. The requested property-testing dependency is `hegeltest`, but dependency selection/configuration belongs in a later task unless needed for this foundation.

The checked-in corpus at `docs/test-corpus/` is authoritative for supported parser behavior. The Rust corpus tests must remain passing before and after changes.

## Scope

1. Turn the Cargo package into a clean library-plus-binary shape suitable for a functional core / imperative shell design. Keep `src/main.rs` as the binary entry point and add a library module (for example `src/lib.rs`) with clearly separated domain types and error types.
2. Define stable internal data structures for:
   - parsed conflict regions and byte ranges;
   - ordered terms, including ordinal, `side`/`base` kind, label, logical bytes, and whether a synthetic separator EOL was removed;
   - a parsed/materialized document and its resolved scaffold;
   - manifest data, source path, source hash, term metadata, conflict ranges, and format/schema version.
3. Define an error type that can distinguish invalid input, unsupported style, missing/unreadable files, external-command failure, invalid manifest/resolved data, and unsafe write conditions. Error messages must be actionable and identify the path/region/byte offset where possible.
4. Define pure-function signatures for parsing/materialization and apply validation without implementing their algorithms yet. Keep IO and process execution out of these functions.
5. Define CLI argument/config types for the two commands and their options, including `--write`, but make the binary report a clear “not implemented” error for commands that do not yet have behavior.
6. Add minimal unit tests for data invariants that do not depend on the parser (for example contiguous term ordinals, required schema version, and error display stability).

## Acceptance criteria

- `cargo fmt --check` and `cargo check` pass.
- `cargo test` passes.
- The library exposes a coherent API that later tasks can use without moving core logic into `main.rs`.
- No external dependency is added without an explicit decision recorded in code/documentation; standard library is preferred.
- Existing corpus files are unchanged and the Rust test suite passes.
- Error output is written to stderr by the eventual shell boundary, but domain errors themselves remain testable without capturing process output.

## Handoff

At the end, report the files changed, the public/internal types and pure-function signatures established, commands run, and any API decision that later tasks must honor. Do not implement later task behavior just to make a demo work.
