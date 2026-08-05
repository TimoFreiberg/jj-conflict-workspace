# jj-conflict-workspace

`jcw` materializes a JJ snapshot conflict into a private workspace that a coding agent can inspect and edit. Version 1 is deliberately conservative: it preserves bytes and line endings, validates every proposal against the original source and manifest, defaults to dry-run, and changes the source only with explicit `--write`.

## Workflow

Run one command at a time:

```text
jcw prepare --file path/to/file [--output-dir DIR]
```

Edit the ordinary structural seed at `<workspace>/resolved`, then preview it:

```text
jcw apply --resolved-file <workspace>/resolved
```

Install the validated result only when ready:

```text
jcw apply --resolved-file <workspace>/resolved --write
```

`prepare` invokes the installed `jj` executable with separate arguments, using `ui.conflict-marker-style=snapshot`, from the nearest ancestor containing a real `.jj` directory. `jcw` does not intentionally mutate the source: it captures the source before invoking JJ and checks its bytes and identity immediately afterward. If external JJ hooks or another process changed or replaced the source, prepare reports the change and does not roll it back; it also creates no workspace or manifest from stale bytes. The workspace is retained after success and after `apply`, so its manifest and numbered artifacts remain available for review.

## Output and exit codes

Paths in output are encoded reversibly so each successful prepare result is exactly one line:

```text
ENCODED_ABSOLUTE_WORKSPACE_PATH\n
```

Printable ASCII other than `%` is literal. `%`, controls, non-printable bytes, and invalid Unix path bytes are `%HH` with uppercase hexadecimal; valid UTF-8 non-ASCII text is preserved. This keeps output to one line while retaining a decoder-compatible representation.

Successful dry-run output is:

```text
Proposed changes for source `ENCODED_SOURCE_PATH`:
<deterministic byte-safe diff>
No files were modified (dry-run).
```

Successful write output is:

```text
Applied N change(s) to `ENCODED_SOURCE_PATH` (OLD bytes -> NEW bytes).
```

`--help` writes only `USAGE` to stdout and exits 0. Syntax errors write no stdout, an actionable diagnostic followed by `USAGE` to stderr, and exit 2. Operational, JJ, validation, stale-source, and write errors write no stdout, an actionable diagnostic to stderr, and exit 1. A post-commit durability or cleanup ambiguity is reported as an error and never as a successful application.

## Workspace layout

A successful workspace has private permissions where the platform supports them:

```text
<workspace>/
  source
  resolved
  manifest.json
  regions/
    region-000/
      term-000.term
      term-001.term
```

`source` is the exact source byte snapshot. `resolved` is an ordinary byte file initialized structurally, not semantically. Each conflict region is represented by `regions/region-NNN/term-NNN.term`; term files contain the exact logical bytes for the corresponding snapshot term. `manifest.json` records schema/version, canonical source identity, repository-relative path, source SHA-256 and length, marker widths, region ranges, term labels/kinds, exact lengths, final-newline metadata, digests, and generated artifact paths.

The resolved seed uses each region's first logical term. It is not a merge result and does not choose a winner. Agents may replace a region with empty bytes, delete it, or add bytes, but bytes outside the recorded conflict ranges must remain unchanged.

## Supported boundary

V1 supports JJ **Snapshot** conflict materialization, arbitrary section arity including repeated bases, multiple regions, custom UTF-8 labels, invalid UTF-8 payload bytes, CRLF and mixed EOLs, and files without a final newline. The installed JJ CLI is the source of truth; this tool does not use `jj-lib` and does not implement a merge algorithm.

The default/diff style, Git/diff3 markers, structural continuation styles, malformed snapshots, and ambiguous metadata are intentionally unsupported. Unsupported input fails conservatively rather than being guessed into terms. Marker-like payload lines shorter than the active marker width remain payload; active-width marker runs in a resolved file are rejected.

## Safety and platform notes

Dry-run is read-only: it validates the manifest, source, resolved file, and every numbered artifact, then renders a deterministic diff without changing the source. `--write` is the only source mutation path. Writes use a complete private sibling replacement, preserve supported source permissions and timestamps, sync the replacement before atomic installation, and keep the workspace. Rejected validation and pre-commit write failures guarantee that the destination was not intentionally changed and report cleanup status. If the platform reports an uncertain commit result, the tool reports that the replacement **may already be present** and tells the operator to inspect both source and workspace.

On Unix, supported Linux/Android and macOS/iOS targets open final regular files with the platform no-follow flag, reject detectable symlinks and non-regular files, and recheck the opened-handle device/inode/type/size/mode/mtime identity immediately before installation. Parent components are checked path-wise with `symlink_metadata` and canonical directory identity capture; they are not all opened with no-follow handles. Consequently, cooperative substitutions detected by these checks are refused, but portable path-based races outside the final recheck cannot be ruled out. Unix targets without the required standard no-follow flag refuse `--write` before mutation. On Windows, dry-run uses available standard path/metadata checks, while `--write` is refused before creating a replacement because this version does not yet provide the required native reparse-point, file-index, and directory-handle identity implementation. Other targets likewise refuse `--write` before mutation when identity-safe replacement is unavailable. Crash durability can still depend on filesystem and operating-system guarantees after an atomic rename.

Workspace creation rejects file and symlink collisions and uses a fresh random child name. Partial workspace failures attempt cleanup and explicitly report whether a partial workspace remains. Do not treat a retained partial workspace as a successful result.

## Verification

The normal no-helper suite is:

```text
cargo fmt --check
cargo check
cargo test
```

The full native-helper integration suite is:

```text
cargo test --features test-support
```

A production release build excludes the test helper:

```text
cargo build --release --no-default-features
```

The test helper is an internal Cargo target used only by the feature-gated tests; it is not a user command and normal operation resolves the installed `jj` executable.
