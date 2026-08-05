# Task 02 — Implement the pure snapshot parser and scaffold materializer

## Prompt

You are implementing the second chunk of `jj-conflict-untangler`. Task 01 established the Rust domain types and pure-function boundaries. Work on the functional core only: do not invoke `jj`, create tempdirs, parse CLI arguments, or write repository files in this task.

## Product and format contract

The tool helps coding agents resolve JJ conflicts by exposing complete numbered terms and a normal `resolved` file. v1 deliberately supports only JJ snapshot-style conflict materialization and arbitrary conflict arity. It must reject other styles conservatively rather than guessing.

Supported bytes grammar:

- An opening conflict line starts with a run of `<` bytes of width `W`, where `W >= 7`; any bytes after that run are part of the opening label/text and must not affect `W`.
- Within a region, section headers start with exactly the same-width run of `+` (side) or `-` (base), followed by an arbitrary UTF-8 label. The first section must be a side; there must be at least two sections; any side/base ordering after that is accepted as JJ materialized order.
- The closing line starts with a run of `>` bytes of exactly width `W` and ends the region. It may or may not have an EOL.
- A marker-like payload line shorter than `W` is ordinary payload, not a marker. Long marker widths, custom labels, empty terms, CRLF, missing final newlines, and multiple regions are valid.
- Structural `%`, backslash continuation, `|||||||`, `=======`, mixed header styles, malformed regions, and wrong-arity/reference examples are not v1-supported. Return a typed, actionable error with region/offset context.

The corpus is authoritative. Its parser reference implementation in `docs/test-corpus/validate.py` documents byte iteration, line splitting, synthetic separator EOL handling, and scaffold reconstruction. Supported fixtures include:

- `snapshot-basic-2-sided`
- `snapshot-3-sided-with-multiple-bases`
- `snapshot-multiple-regions`
- `snapshot-long-markers-and-marker-like-content`
- `snapshot-missing-final-newlines`
- `snapshot-crlf`
- `snapshot-custom-labels`
- `snapshot-empty-term-or-deletion`

Logical term bytes are the bytes between a section header and the next section header/closing marker. If the closing marker has no EOL, JJ renders a separator EOL after each section; remove exactly one such separator from each logical term. Do not strip arbitrary whitespace or normalize line endings. An explicitly empty term is a valid zero-byte artifact.

## Scope

1. Implement the pure byte parser against `&[u8]` (or an equivalent owned representation), with no filesystem or process calls.
2. Track exact byte ranges for each region and the source document. Ranges must support the later invariant “bytes outside all original regions are unchanged.” Ensure regions cannot overlap and are returned in source order.
3. Extract ordered terms with labels and kinds. Preserve labels as bytes or validated UTF-8 according to the API from Task 01; invalid UTF-8 must produce a clear parse error rather than lossy replacement.
4. Implement logical payload extraction, including CRLF-aware line handling and the missing-final-newline synthetic separator rule.
5. Implement scaffold reconstruction: copy all bytes outside each region verbatim and replace each region with its first logical term payload. Preserve all outside bytes and the selected term bytes exactly.
6. Reject unterminated regions, no-region input, marker width below seven, mismatched closing width, missing/invalid section sequences, unsupported structural marker families, and malformed labels with typed errors.
7. Add focused unit tests for each grammar rule and byte edge case. Keep tests deterministic and independent of `jj`.

## Important implementation constraints

- Do not parse by splitting on literal strings such as `<<<<<<<` or `=======`; marker width and line boundaries matter.
- Do not interpret `to:` lines, escaped backslashes, or diff-style payloads as complete terms.
- Do not use `String::from_utf8_lossy`, `lines()`, `read_to_string`, or APIs that discard CRLF/no-EOL distinctions.
- Avoid panics on arbitrary bytes. Every malformed input path must return an error.
- Keep the parser pure and cheap to fuzz/property-test. It should not mutate inputs.

## Acceptance criteria

- All supported corpus inputs parse and reconstruct to their checked-in `resolved` bytes.
- The parser preserves every declared term’s bytes, ordinal, kind, label, and synthetic-separator fact.
- CRLF and no-final-newline fixtures remain byte-exact.
- Reference fixtures are rejected with the intended unsupported/malformed/wrong-arity distinction where the contract makes that distinction possible.
- `cargo fmt --check`, `cargo check`, and `cargo test` pass; `python3 docs/test-corpus/validate.py` still passes.

## Handoff

Report the pure functions added, error cases, corpus cases exercised, and any ambiguity in JJ’s grammar. Leave process/IO integration to later tasks.
