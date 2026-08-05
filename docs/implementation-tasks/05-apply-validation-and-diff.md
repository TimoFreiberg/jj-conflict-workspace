# Task 05 — Implement pure guarded-apply validation and diff planning

## Prompt

You are implementing the fifth chunk of `jj-conflict-workspace`. Tasks 01–04 provide the domain model, pure parser, tests, and `prepare` artifacts. Implement the pure validation/planning core for `apply`; do not write the repository source or perform atomic replacement yet.

## Product contract

`apply` receives a workspace `resolved` file and its sibling `manifest.json` (with an explicit `--manifest` override allowed). It is dry-run by default. Before it can show or install anything, it must prove that the proposal belongs to the unchanged source and only resolves originally conflicted bytes.

Required checks:

1. The current source file’s bytes still hash to the manifest’s recorded source hash. Reject stale workspaces without reading the proposal as an installable change.
2. The resolved bytes contain no JJ conflict markers or helper placeholders: reject `<<<<<<<`, `>>>>>>>`, `%%%%%%%`, `+++++++`, `|||||||`, `=======`, and any configured JJ metadata/continuation labels. This check must be byte-aware and avoid rejecting ordinary content solely because it contains a short marker-like line when the contract says it is ordinary content.
3. Every byte outside the original conflict regions is identical to the original source. This is the critical guard against an agent editing unrelated code in `resolved`.
4. The proposal is structurally compatible with the manifest’s region ranges and source length/encoding-independent byte model. Handle length changes inside regions; do not assume line counts are stable.
5. Produce a deterministic diff plan/representation for the changed ranges. Do not mutate files while computing it.

## Scope

1. Implement manifest loading/validation as a pure data-validation layer after the shell reads bytes. Validate schema version, source path identity, source hash format, contiguous region ordering/non-overlap, and term metadata consistency. Reject path traversal or artifact paths escaping the workspace.
2. Implement a pure function that accepts manifest + original source bytes + resolved bytes and returns either a validated apply plan or a typed rejection. Include changed byte ranges and enough context for a human/agent-readable unified diff.
3. Implement exact outside-region comparison. Prefer prefix/suffix/range comparisons that report the first offending byte and the corresponding region/line range; do not “normalize” whitespace, EOLs, Unicode, or final newlines.
4. Implement marker/placeholder detection based on the v1 contract. A resolved file must be an ordinary complete file, but legitimate marker-like payload content from the original terms must not be rejected merely because it is shorter than the active marker width. Make the policy explicit and unit-test it.
5. Implement a diff renderer or a narrow internal diff plan sufficient for the shell to print a clear proposed change. It must handle empty files, insertions, deletions, CRLF, and no-final-newline without panicking. If a standard-library-only line diff is used, retain byte safety and label no-final-newline cases clearly.
6. Add exhaustive unit tests for valid edits confined to one/multiple regions and every rejection path, including stale source, changed prefix/suffix, marker-bearing resolved data, malformed manifest, overlapping ranges, and length changes.

## Important constraints

- Do not reconstruct a merge or compare semantic terms; the agent owns semantic resolution.
- Do not accept a resolved file merely because it has no obvious markers; the outside-region invariant is mandatory.
- Do not use a text decoding that can alter bytes. Any display conversion must use an explicit safe representation for invalid UTF-8.
- Do not install, rename, truncate, or chmod the source in this task.

## Acceptance criteria

- Pure validation is callable and fully testable without filesystem/process access.
- A valid proposal yields a deterministic plan/diff; every invalid proposal yields a typed actionable error and no plan.
- Tests prove that unrelated edits are rejected even when the edited file is syntactically conflict-free.
- `cargo fmt --check`, `cargo check`, and `cargo test` pass.

## Handoff

Report the validation invariants, marker policy, diff format, and error-context guarantees. The next task will wire the plan to dry-run output and atomic writing.
