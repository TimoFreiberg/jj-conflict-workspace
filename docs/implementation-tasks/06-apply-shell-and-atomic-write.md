# Task 06 — Wire `apply` dry-run and guarded atomic installation

## Prompt

You are implementing the sixth chunk of `jj-conflict-untangler`. Tasks 01–05 established the library contract, pure snapshot core, corpus/property coverage, `prepare`, and pure guarded-apply validation/diff planning. Implement the imperative shell for `apply` now. Do not broaden the parser or turn this into a merge engine.

## Product contract

The agent workflow is:

```text
jj-conflict-untangler prepare --file path/to/file
# edit the ordinary workspace/resolved file
jj-conflict-untangler apply --resolved-file /absolute/workspace/resolved
# dry-run: validate and print proposed diff; source remains unchanged
jj-conflict-untangler apply --resolved-file /absolute/workspace/resolved --write
# repeat validation, then install only after all checks pass
```

The manifest is normally the sibling `manifest.json`; `--manifest MANIFEST` is an explicit override for humans/tests. Apply must refuse to guess a manifest when the relationship is ambiguous. The workspace remains intact after dry-run and successful write.

## Scope

1. Add CLI dispatch and argument validation for `apply --resolved-file FILE [--manifest FILE] [--write]`. Reject missing values, unknown flags, duplicate incompatible options, directories, and unreadable artifacts with concise actionable diagnostics.
2. Discover/validate the manifest and source paths safely. Resolve paths without following generated-artifact symlink tricks; ensure the explicit manifest and resolved file belong to the same intended workspace unless the manifest contract explicitly allows a safe external location.
3. Read manifest, current source, and resolved bytes, then call the pure validation/planning function from Task 05. Do not duplicate its checks in the shell. Surface stale-source, marker, outside-region, and malformed-manifest errors with path and region context.
4. In dry-run mode (default), print the proposed deterministic diff and an explicit statement that no files were modified. Exit nonzero on every validation failure; do not print a successful diff for a rejected plan.
5. With `--write`, rerun all validation immediately before mutation. Preserve source file permissions, mode, executable bit, and relevant metadata as far as the platform permits. Do not alter the file if the second validation fails.
6. Install bytes atomically: write the complete replacement to a securely created temporary sibling file, flush/sync as appropriate for the platform, preserve permissions, then atomically rename/replace the source. Handle Windows/Unix differences through a small explicit abstraction or a documented safe fallback; never truncate the source before the replacement is ready.
7. If any write/rename/sync step fails, report the exact phase and paths, keep the original source intact whenever the platform guarantees that, and never claim success. Do not delete the workspace or resolved file.
8. After successful `--write`, print the source path and a concise applied-change summary. Do not invoke `jj` to update repository metadata; the helper’s responsibility ends at guarded file replacement.

## Safety requirements

- `--write` is the only path allowed to mutate the source.
- Dry-run must be observational: tests should snapshot source bytes, permissions, and modification behavior where portable.
- Repeat `--write` with the same resolved bytes should be safe and produce no unintended changes.
- Never follow a symlink at the source path if doing so could replace a different target than the manifest identifies; make the policy explicit and test it.
- Preserve zero-byte files, CRLF, missing final newline, and invalid UTF-8 bytes exactly.
- Never use shell commands for file replacement or interpolate paths into shell strings.

## Acceptance criteria

- Integration tests cover default dry-run, `--write`, explicit/sibling manifest discovery, stale source between prepare and apply, invalid marker-bearing resolution, unrelated outside-region edits, no-op resolution, empty replacement, CRLF/no-final-newline, permission preservation, and simulated atomic-write failure.
- Tests prove dry-run and every rejected apply leave source bytes unchanged.
- Tests prove a successful write changes only the intended source file and leaves workspace artifacts available.
- `cargo fmt --check`, `cargo check`, `cargo test`, and `python3 docs/test-corpus/validate.py` pass.

## Handoff

Report the CLI output contract, path/symlink policy, atomic replacement strategy per supported platform, and failure guarantees. Do not add cleanup behavior unless it is strictly optional and cannot remove a workspace unexpectedly.
