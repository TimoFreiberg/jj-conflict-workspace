# Task 07 — Harden the complete CLI and document the agent workflow

## Prompt

You are implementing the seventh and final chunk of `jj-conflict-workspace`. Tasks 01–06 provide the parser, tests, `prepare`, pure apply validation, dry-run, and guarded atomic installation. Review and harden the complete tool for real coding-agent use. Do not redesign the product or add a merge algorithm.

## Product contract to preserve

- The tool is a conflict materializer and guarded installer, not a semantic merge tool.
- v1 supports JJ snapshot-style markers only, with arbitrary term arity and numbered `term-NNN` artifacts.
- Complete term snapshots come from the installed `jj` CLI; v1 does not depend on `jj-lib`, avoiding version skew.
- `prepare` writes an ephemeral private workspace in the platform temp directory and never changes the repository.
- `resolved` is an ordinary structural seed using each region’s first logical term; the agent edits it.
- `apply` is dry-run by default and requires `--write` to mutate the source.
- Apply requires the original source hash, marker-free resolved bytes, byte-for-byte preservation outside original conflict regions, a readable deterministic diff, and atomic replacement.
- All bytes/EOLs/permissions and arbitrary conflict arity matter. Fail conservatively and explain failures clearly.

## Scope

1. Perform an end-to-end code review of the CLI and library for accidental panics, unchecked indexing, lossy UTF-8 conversion, newline normalization, path traversal, symlink races, shell interpolation, temporary-file collisions, and misleading success messages. Fix issues found within the established design.
2. Exercise the complete workflow with deterministic integration fixtures and a fake `jj` executable: prepare a source, inspect terms/manifest, edit resolved, dry-run apply, mutate the source to force a stale rejection, then successfully apply with `--write`.
3. Add adversarial tests for arbitrary marker widths, marker-like payload lines, custom/non-ASCII labels where supported, five or more terms with multiple bases, multiple regions, empty terms/deletions, CRLF, no final newline, empty source/resolution, malformed JJ output, unreadable paths, output-directory collisions, and failure during artifact creation or atomic replacement.
4. Check CLI exit codes and stdout/stderr separation. Stdout must be machine-consumable and concise; stderr must contain actionable diagnostics. Ensure errors identify the operation, path, region/offset when available, and remediation (for example rerun prepare or use `--write`).
5. Make the manifest schema and workspace layout self-documenting. Add or update a concise project README/usage document with the exact commands, expected artifacts, supported v1 format, dry-run behavior, `--write` safety, workspace retention, and the fact that the first term is only a structural seed.
6. Document intentionally unsupported styles and the conservative failure behavior. Explain that default JJ diff-style output is not parsed as complete terms and that future support may require a versioned design change.
7. Review dependencies and build configuration. Keep the standard library where practical; do not add `jj-lib`. If `hegeltest` is used, ensure its purpose, version constraint, and approval are recorded and that normal offline tests/builds have a clear policy.
8. Run the full verification matrix and fix regressions rather than weakening tests:
   - `cargo fmt --check`
   - `cargo check`
   - `cargo test`
   - `cargo build --release`
   - representative manual CLI runs using a fake JJ command, including failure paths
9. Inspect generated artifacts for accidental repository writes, secret/environment leakage, unstable absolute paths in checked-in files, and overly verbose output. Keep temporary workspaces out of version control and confirm `.gitignore`/equivalent coverage for build output only, not source fixtures.

## Acceptance criteria

- The full test and corpus validation matrix passes.
- End-to-end tests demonstrate that the repository source is unchanged after prepare and dry-run, changed only after explicit successful `--write`, and unchanged after every rejected or failed operation.
- The docs are sufficient for a fresh coding agent to run the workflow using one command at a time and understand which file to edit.
- Unsupported input is rejected with a clear typed/user-facing error rather than guessed into potentially wrong terms.
- No known panic, data-loss path, or silent byte normalization remains in the reviewed workflow.

## Final handoff

Report the final command syntax, workspace artifact layout, supported/unsupported format boundary, dependency decisions, verification commands and results, and any explicitly remaining platform limitations. This task is complete only when the implementation is safe to hand to coding agents, not merely when it compiles.
