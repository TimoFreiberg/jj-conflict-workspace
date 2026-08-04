# JJ conflict test corpus

This directory is a **one-time vendored snapshot** of selected JJ conflict test
inputs and expected bytes. It is not a submodule, Cargo dependency, build-time
checkout, runtime JJ invocation, or continuously synchronized copy. It remains
usable after the upstream checkout is deleted.

## Provenance and scope

The source is the Apache-2.0 JJ repository at https://github.com/jj-vcs/jj, pinned to commit
`6b27ec86af32ff84c209367d2710d234cb192622`. See [PROVENANCE.md](PROVENANCE.md) and the local
[JJ-LICENSE-APACHE-2.0.txt](JJ-LICENSE-APACHE-2.0.txt). Fixtures are adapted
test data, not copied Rust test harness code. Stable library labels are kept;
volatile operation/revision identifiers are not copied. Rust indentation and
literal escapes were normalized as documented in each `case.json`.

## Layout

- `cases/<name>/input.snapshot` is a materialized snapshot-style input.
- `cases/<name>/regions/region-NNN/term-NNN.term` contains one logical term per
  region in materialized order; section marker/header/label lines are excluded.
- `cases/<name>/resolved` is a structural scaffold made by replacing every
  conflict region with that region's first logical term (`term-000`) while
  preserving stable bytes outside the regions. It is not a semantic resolution.
- `reference/<name>/input.snapshot` and its `case.json` document rejected or
  reference-only formats: default diff, Git/diff3, malformed, wrong-arity, and
  whitespace-tolerant inputs. They intentionally have no resolved scaffold.
- `index.json` lists all cases; each case's `case.json` artifact map is
  authoritative for byte lengths, hashes, EOL mode, final-newline state, and
  synthetic separator-EOL facts.

The future test loader can therefore read one input, enumerate each region's
contiguous terms, and compare a materializer's resolved scaffold byte-for-byte.
Supported cases use a deliberately narrower v1 grammar: snapshot section headers
only (`+++++++` sides and `-------` bases), with the outer marker width inferred
from the opening marker. Diff/Git inputs are references rather than supported
behavior.

## Exact-byte policy

Files are read and written as bytes. The corpus-local `.gitattributes` disables
text/EOL normalization for extensionless `resolved` and `term-NNN.term` files
as well as snapshot inputs. CRLF fixtures contain actual `\r\n` bytes, missing-final-
newline fixtures really omit the final newline, and empty terms are zero-byte
files. A rendered section may contain JJ's synthetic separator EOL when a
closing marker lacks its final newline; this is recorded per term in metadata.
The long-marker case uses sixteen-character outer markers while preserving
shorter marker-like payload lines.

## Validation

Run the standard-library-only validator from the repository root:

```sh
python3 docs/test-corpus/validate.py
```

It checks layout and hashes, snapshot semantics and reconstruction, byte
properties, documentation/provenance, symlinks/git attributes, and absence of a
live JJ checkout dependency. It never invokes JJ and requires no external
checkout or Cargo dependency.
