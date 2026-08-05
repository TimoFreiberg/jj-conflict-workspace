# Task 03 — Build exhaustive corpus and property-based coverage

## Prompt

You are implementing the third chunk of `jj-conflict-workspace`. Tasks 01–02 established the Rust core types and pure snapshot parser/materializer. Work primarily on tests and test support. Do not add CLI process execution, tempdir creation, or repository writes here.

## Why this task exists

This tool must never silently produce misleading term files or install an unsafe resolution. The parser is byte-oriented and has subtle cases: arbitrary marker widths, arbitrary numbers of terms/bases, multiple regions, empty terms, CRLF, missing final newlines, custom labels, and marker-like payload content. The vendored corpus under `docs/test-corpus/` is self-contained, pinned to JJ commit `6b27ec86af32ff84c209367d2710d234cb192622`, and its validator is authoritative. Do not fetch a live JJ checkout or alter fixture bytes/metadata unless a genuine defect is found and documented.

The supported cases are the eight `snapshot-*` cases listed in `docs/test-corpus/README.md`. The reference cases intentionally cover default diff, Git/diff3, malformed, wrong-arity, and whitespace-stripped formats and should remain rejected by v1.

## Scope

1. Add a Rust test loader for the corpus using byte reads and JSON metadata. Prefer a small test-only dependency only after asking the operator; otherwise implement the narrow metadata extraction needed with the standard library or a carefully justified dependency decision. Do not add `jj-lib`.
2. For every supported case, assert:
   - parser region count and term count;
   - term kind, ordinal, label, logical bytes, and synthetic separator flag;
   - exact scaffold bytes equal the checked-in `resolved` artifact;
   - all bytes outside conflict ranges equal the original input.
3. Add tests for every reference case’s expected disposition. Tests must assert rejection and should check the error category/message contains useful context without overfitting unstable wording.
4. Add property-based tests with the requested `hegeltest` dependency if the operator has approved it. Properties should cover generated arbitrary-width snapshot regions, arbitrary term counts (including multiple bases), arbitrary payload bytes, multiple regions, CRLF/no-EOL combinations, and marker-like payload lines shorter than the active width. If the dependency is not approved/available, implement deterministic generator-based tests now and leave a clearly isolated seam for later hegeltest integration.
5. Add negative properties: truncating any closing marker, removing a required section header, changing the close width, and introducing mixed structural header styles must never yield a successful misleading parse.
6. Ensure tests do not rely on locale, platform newline conversion, a live `jj`, `/tmp`, or file ordering.

## Exact-byte requirements

- Use `fs::read`-equivalent byte reads, not text reads.
- CRLF bytes must remain CRLF in outside content and terms.
- A closing marker without EOL causes exactly one rendered separator EOL to be removed from each section payload; no other byte may be trimmed.
- Empty term means an existing zero-byte term and is valid.
- The first term is used only for structural scaffold generation; tests must not imply it is a semantic merge choice.

## Acceptance criteria

- `cargo test` runs the full supported corpus suite and the negative/reference suite.
- Property tests run reliably under normal `cargo test` without network access and with useful shrinking/minimal failing examples if hegeltest is used.
- Tests cover arbitrary term arity rather than hardcoding three terms; specifically include five-term/two-base behavior.
- `cargo fmt --check`, `cargo check`, and `cargo test` pass.
- Test names and failure messages identify case, region, and term indices.

## Handoff

Report test counts and the properties covered, identify any fixture disposition that is intentionally not asserted, and mention whether `hegeltest` was added/approved. Do not weaken parser behavior to make a test pass.
