I want to create a CLI utility in this repo, That is intended to help coding agents solve jj conflicts. The usability should be tailored towards the unique circumstances of coding agents. So human UX is not the priority, agent UX is the priority. 
In a nutshell, the script should be run when the current JJ checkout contains conflicts and it should create a temporary directory containing files that represent a view of the conflicts that is easier for the agent to read and especially easier to modify. The background for this task is that the conflict conflict markers that JJ leaves in the source files are very hard for the agent to edit accurately.

They frequently make editing mistakes, they miscount number of sigils that separate the different merge bases of the conflict marker.  The idea is that the script parses the conflict files or it uses JJ to produce the version of the conflict files that is easiest to parse the CLI utility creates separate files for each base file and instructs the agent to read those files giving it full absolute file path and instructs it to create a resolved file and gives instructs it um with the right invocation to call the script again with with the resolved file.

The resolved file is then used by the CLI And put in place in the real conflicted report again. either directly run the right jj commands to resolve or it can just print the right commands. I think it's probably best if the CLI utility does the least work possible so it's easier to compose. I think the best way to view the CI utility is to have it translate a JJ produced conflict file into a untangled view of that conflict with separate files per conflict base. The agent's task is to create the conflict resolution file and it can just read all the conflict base files without having to deal with conflict markers and then the CLI utility has the second function of doing kind of the same transformation backwards, producing, updating the repo based on the resolved file. If that makes sense.

I want this to be a Rust CLI tool that should probably just use the standard library, but if there are reasons to use any external crates, that's fine. I want to be asked about any dependencies though. One dependency I do want to use is the https://docs.rs/hegeltest/latest/hegel/ property based testing library. I want this to be well tested, I want it to be bulletproof, super robust, I want it to never fail in surprising ways. Any failures any uh invalid input should be very clearly communicated to the agent and if it fails to create the temp directory it should give a very clear um error report.
To be able to be well tested, the core algorithm should be a pure function with some straightforward IO around it following the functional core imperative shell approach. This pure functional core should be heavily tested and I want like property-based tests and all the edge cases that an agent can come up with covered. 


following are agent opinions from the initial brainstorming sessions. they're extra context and initial suggestions and don't override my design braindump from before:


<agent>

## Recommended model ##

Treat the script as a conflict materializer and guarded installer, not a merge tool.

For each conflicted path, create a directory like:
.jj-conflicts/
  origind-server__src__server__service.rs/
    manifest.json
    conflicted
    base
    destination
    rebased
    resolved

Where:

• `conflicted` is the original materialized file, retained for diagnostics;
• `base` is the common-base file;
• `destination` is one complete side;
• `rebased` is the other complete side;
• `resolved` is the complete file the agent should author;
• `manifest.json` contains the source hash, path, conflict count, and revision IDs.

The agent never needs to read or edit `conflicted` unless diagnosing the script.

The agent workflow becomes:
script-name prepare --file origind-server/src/server/service.rs

Then:
read base
read destination
read rebased
write/edit resolved

Finally:
script-name resolved \
  --manifest .jj-conflicts/origind-server__src__server__service.rs/manifest.json \
  --resolved-file .jj-conflicts/origind-server__src__server__service.rs/resolved

I would probably call the final command `apply`, but `resolved` is perfectly understandable:
script-name resolved --resolved-file <path>

The manifest can be inferred from the resolved file’s directory if you want the shortest possible agent command.

## Important subtlety: don’t split jj’s marker text naively ##

The separate side files should be complete source snapshots, not merely the raw text between marker lines.

jj’s materialized format is asymmetric. One section may be a snapshot while the other is represented as a diff from a base. If the script simply splits:
+++++++ ...
%%%%%%% ...
to: ...

into files, one “side” may not actually be a complete side.

A small script can likely reconstruct complete sides by:

1. parsing the revision IDs from the conflict labels;
2. running `jj file show -r <revision> <path>`;
3. writing those complete file contents into `destination`, `rebased`, and `base`.

That is still substantially simpler than implementing a merge engine. It only needs to materialize the revisions jj already identifies.

For a first version, it is reasonable to support the common two-sided case and fail conservatively on unsupported conflict shapes:
Error: conflict 2 is not a supported two-sided file conflict.
No files were modified.

A safe refusal is much better than producing misleading side files.

## Seed `resolved` from one side ##

I would create `resolved` initially as a copy of the destination side, rather than as an empty file.

That gives the agent:

• a syntactically complete file;
• all destination-side changes already present;
• a normal file it can edit;
• a natural place to add the rebased-side changes.

For a trivial conflict, the agent may not need to change it at all.

The manifest can record:
{
  "source": "origind-server/src/server/service.rs",
  "source_sha256": "...",
  "destination_revision": "souosymz",
  "rebased_revision": "ntxvssxu",
  "base_revision": "ptuktnku",
  "conflict_count": 2
}

## Guarded installation ##

The `resolved` command should be strict:

1. Check that the current source file still matches `source_sha256`.
2. Check that `resolved` contains no:
  • `<<<<<<<`
  • `>>>>>>>`
  • `%%%%%%%`
  • `+++++++`
  • jj metadata labels.
3. Verify that the resolved file only changes regions that were originally conflicted.
4. Show a unified diff.
5. Atomically replace the source file only after validation.

The third check is especially valuable. If the agent accidentally changes an import, generated definition, or unrelated test while editing `resolved`, the script should reject it:
Error: resolved file changes lines outside conflict regions:
  origind-common/src/codec/protos/protos_api.rs: 812-819

That gives you a strong guarantee that the script is transporting an intentional conflict resolution rather than blindly overwriting the file.

For multiple conflicts in one file, the agent can still author one complete `resolved` file. The script applies it once and checks that all non-conflicted regions remain byte-for-byte unchanged.

## Why this would have helped both sessions ##

This design would have eliminated the observed mechanical failures:

• guessed backslash counts;
• literal `\async`;
• literal `\use`;
• stray `\        }`;
• duplicate imports;
• ambiguous `to:` replacements;
• edits based on stale same-file state.

It would not eliminate semantic mistakes such as the missing `ConnectInfo` derives in `07pvkk-proof`, but that is the right boundary. Compilation and focused tests should catch those.

## Minimal command set ##

I would keep the script to three commands:
# Materialize safe artifacts; do not modify the source.
script-name prepare --file FILE --output-dir DIR

# Check a proposed resolution without modifying the source.
script-name resolved --manifest MANIFEST --resolved-file RESOLVED --check

# Install it after all checks pass.
script-name resolved --manifest MANIFEST --resolved-file RESOLVED --write

Or make `resolved` dry-run by default and require `--write`.

The core idea is excellent: make the agent author a normal file, while the script owns all interaction with jj’s conflict representation. That should be far smaller than a comprehensive jj conflict-resolution helper and directly addresses the failures we saw.

</agent>

<operator>
sounds very close to baked. we need to be generic over the number of sources though because jj can have conflicts with an arbitrary number of bases, so just number the bases imo. i think we should go with tempdirs in /tmp to avoid clobbering the repo though?

and i agree with making the apply command (apply is a good name, we can keep that one) dry-run by default and require a `--write` any reason why it could help making it a rust tool? we could depend on jj natively and import its conflict marker code maybe?
</operator>

<agent>

Yes. I’d make the helper an ephemeral conflict workspace:
/tmp/jj-conflict-<random>/
  manifest.json
  source
  term-000
  term-001
  term-002
  resolved

Use the platform temp directory (`$TMPDIR` on macOS, usually `/tmp` on Linux) through a secure `mkdtemp`-style API rather than hardcoding `/tmp`. Create it with mode `0700`.

## Number terms uniformly ##

I would avoid names like `base`, `ours`, or `theirs` in the filenames:
term-000
term-001
term-002

The manifest can retain whatever labels jj provides:
{
  "source_path": "origind-server/src/server/service.rs",
  "source_sha256": "...",
  "terms": [
    {
      "index": 0,
      "label": "ptuktnku ...",
      "kind": "base"
    },
    {
      "index": 1,
      "label": "ntxvssxu ...",
      "kind": "side"
    }
  ]
}

The agent only needs to reason about numbered complete files. The metadata is useful for humans and diagnostics but should not be required for the agent’s editing decisions.

This handles arbitrary conflict arity and avoids baking in assumptions about jj’s current conflict roles.

## Suggested workflow ##
jj-conflict prepare --file path/to/file

Output:
/tmp/jj-conflict-a8f31c/

The script creates:

• `term-*`: complete snapshots of every conflict term;
• `source`: the original materialized file, for debugging;
• `resolved`: a conflict-free starting point;
• `manifest.json`: hashes, path, conflict ranges, labels, and term count.

I would seed `resolved` by copying the original file and replacing every conflict block with `term-000`. That gives the agent a complete ordinary source file with the surrounding context preserved. It can then read the other `term-*` files and edit `resolved`.

Then:
jj-conflict apply \
  --resolved-file /tmp/jj-conflict-a8f31c/resolved

This should be a dry run and print the proposed diff.

Only this should modify the repository:
jj-conflict apply \
  --resolved-file /tmp/jj-conflict-a8f31c/resolved \
  --write

The manifest can be discovered as the sibling `manifest.json`, so the agent does not need another argument. Supporting an explicit `--manifest` is still useful for humans and tests.

## What `apply` should verify ##

Before showing or applying the diff:

1. The original source path still has the recorded SHA-256.
2. `resolved` has no jj marker families or helper placeholders.
3. The resolved file preserves every byte outside the original conflict ranges.
4. The proposed changes are limited to those ranges.
5. The replacement is written atomically.
6. The source file’s permissions and executable bit are preserved.

The “unchanged outside conflict ranges” check is particularly important. It prevents an agent from accidentally editing unrelated code in `resolved` and having the helper silently install it.

After `--write`, the helper should leave the tempdir intact. It is useful for debugging and review, and the OS can clean it eventually. An optional:
jj-conflict clean --dir /tmp/jj-conflict-a8f31c

would be enough; I would not make cleanup part of the first version.

## Use jj’s snapshot marker style ##

jj already supports a `snapshot` conflict-marker style that displays a complete snapshot for each side/base, unlike the default style’s one snapshot plus diffs. The docs explicitly say this style supports arbitrary-sided conflicts.

That makes the materialization path much simpler:

1. Ask jj to produce the file using `ui.conflict-marker-style=snapshot`.
2. Parse the resulting numbered snapshot sections.
3. Write each section to `term-*`.
4. Avoid interpreting the escaped `to:` continuation line entirely.

The script should not try to reconstruct complete sides from the default diff-style materialization. That is exactly where the current sessions got into trouble.

## Rust: when it helps ##

Rust could help in two ways:

• use jj’s parser/materialization implementation instead of reimplementing marker parsing;
• provide robust byte-level file handling, hashes, permissions, and atomic replacement in one portable binary.

The public `jj-lib` crate does expose conflict APIs such as:
parse_conflict(...)
materialize_merge_result(...)
materialize_merge_result_to_bytes(...)

So reusing jj’s parsing code is a legitimate possibility.

## Why I would not start with `jj-lib` ##

There are important costs:

• `jj-lib` is closely coupled to jj’s release version;
• its APIs are more like jj implementation libraries than a stable plugin interface;
• `parse_conflict()` works with `Merge<BString>` values but does not appear to expose all the source-label semantics needed by itself;
• loading a jj repository and resolving revisions through `jj-lib` is considerably more involved than invoking the installed `jj`;
• the helper could accidentally use jj-lib 0.43 behavior while the user has a different jj binary installed.

The biggest concern is version skew:
installed jj behavior ≠ helper's jj-lib behavior

That is undesirable for a tool whose purpose is safe conflict transport.

## My recommendation ##

For v1:

• write a small Rust or Python executable;
• invoke the installed `jj` CLI to obtain snapshot-style conflict materialization;
• parse only the simple snapshot representation;
• keep all artifacts in a secure OS tempdir;
• use `term-000`, `term-001`, etc.;
• seed `resolved` from one term;
• make `apply` dry-run by default;
• require `--write`;
• guard application with hashes and an outside-conflict invariant.

If you specifically want a single self-contained binary and robust byte handling, Rust is reasonable. But I would use Rust without depending on `jj-lib` initially. Let jj remain the source of truth for materialization, and keep the helper responsible only for temporary files, snapshot extraction, validation, and guarded installation.

That keeps the project small while leaving the option to replace the parser with `jj-lib` later if snapshot parsing turns out to be the fragile part.
</agent>
