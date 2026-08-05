# Task 04 — Implement `prepare` and secure temporary workspaces

## Prompt

You are implementing the fourth chunk of `jj-conflict-untangler`. Tasks 01–03 established and tested the pure snapshot core. Implement the imperative shell for `prepare`; do not implement guarded source installation yet.

## Product contract

`prepare` translates a JJ-conflicted source into an ephemeral workspace for a coding agent without changing the repository. The intended workflow is:

```text
jj-conflict-untangler prepare --file path/to/file
# prints the absolute workspace path
# workspace contains:
#   manifest.json
#   source
#   term-000, term-001, ...
#   resolved
```

The workspace must use the platform temp directory (e.g. `$TMPDIR` on macOS, usually `/tmp` on Linux) via secure random creation, not a hardcoded path. Restrict permissions to the creating user as far as the platform permits. Leave the workspace intact after successful prepare and after later apply so an agent can inspect/debug it.

v1’s source of truth is the installed `jj` executable, not a bundled `jj-lib` version. Snapshot-style materialization must be requested from JJ. The implementation must make the exact command/configuration explicit and must capture stdout/stderr and exit status. Do not silently parse default diff-style output as complete terms. If JJ cannot be found, exits nonzero, emits invalid snapshot bytes, or output cannot be associated safely with the requested file, return a clear error and avoid leaving a misleading partial workspace.

## Scope

1. Add the CLI dispatch for `prepare --file FILE [--output-dir DIR]` using the contract from Task 01. Support paths with spaces and non-UTF-8 paths where the platform/API allows; do not shell-interpolate user input.
2. Validate the requested source path before invoking JJ: it must be a regular file, readable, and represented safely in the current repository context. Record its repository-relative path when possible; reject ambiguous/outside paths with a useful error.
3. Invoke `jj` as a child process with separate arguments and snapshot conflict-marker configuration. Do not use `sh -c`. Preserve command diagnostics and include the command/exit status in errors without leaking irrelevant environment secrets.
4. Read the original source bytes and parse the snapshot bytes through the pure core. If the current source is not a supported snapshot conflict file, fail conservatively before creating a final workspace.
5. Create a private workspace, write `source`, one top-level `term-NNN` file per logical term (or use the API’s documented per-region layout if Task 01 chose it), `resolved`, and `manifest.json`. The manifest must record schema version, absolute/canonical source identity as appropriate, repository-relative source path, source hash, region ranges/count, marker metadata, and all term labels/kinds/hash/length/EOL/synthetic-separator facts required by `apply`.
6. Make artifact creation failure-safe: do not claim success until all files and manifest are complete and flushed as needed. On failure, report the workspace path if useful for diagnostics and clearly state whether cleanup occurred. Never replace the source.
7. Print only a concise, absolute workspace path plus actionable next steps on stdout; diagnostics go to stderr. Ensure output is stable enough for an agent to consume.

## Safety and byte rules

- Use byte reads/writes; preserve exact bytes and permissions where relevant.
- Reject symlink tricks or path traversal in any generated artifact name.
- Use deterministic zero-padded term numbering if that is the API contract; arbitrary arity must work.
- A zero-byte term must still be created as an existing file.
- Do not delete an existing caller-supplied `--output-dir`; create a new child workspace and refuse unsafe collisions.

## Acceptance criteria

- Integration tests use a fake `jj` executable/script in a temporary PATH or an injectable command runner; they never require a developer’s real JJ repository.
- Tests cover success, missing executable, nonzero JJ exit, malformed/unsupported output, spaces in paths, CRLF/no-final-newline, arbitrary term counts, empty terms, and cleanup after partial failure.
- Successful prepare leaves source bytes unchanged and produces artifacts that match the corpus semantics.
- `cargo fmt --check`, `cargo check`, `cargo test`, and `python3 docs/test-corpus/validate.py` pass.

## Handoff

Report the exact JJ invocation/configuration, workspace layout, manifest schema, failure-cleanup policy, and how the command runner is tested. Leave `apply --write` to later tasks.
