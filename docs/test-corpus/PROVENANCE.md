# Provenance

This corpus is derived from selected test data and conflict-format documentation
in the Apache-2.0 [JJ repository](https://github.com/jj-vcs/jj), pinned immutably to
`6b27ec86af32ff84c209367d2710d234cb192622`. The pinned source links below are commit-qualified and are intended as
human provenance only; no fixture or validator requires the source checkout.

## Source links

- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L89-L106>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L133-L155>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L295-L320>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L483-L580>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L645-L693>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L854-L908>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1104-L1141>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1404-L1447>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1450-L1472>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1521-L1543>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1546-L1566>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1613-L1637>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1640-L1679>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L1948-L2107>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/tests/test_conflicts.rs#L2111-L2237>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/src/conflicts.rs#L1335-L1408>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/lib/src/conflicts.rs#L838-L1060>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/cli/tests/test_resolve_command.rs#L1577-L1605>
- <https://github.com/jj-vcs/jj/blob/6b27ec86af32ff84c209367d2710d234cb192622/docs/conflicts.md#L1-L220>

## Case mapping

- `snapshot-basic-2-sided`: `lib/tests/test_conflicts.rs::test_materialize_conflict_basic` lines 133-155
- `snapshot-3-sided-with-multiple-bases`: `lib/tests/test_conflicts.rs::test_materialize_conflict_three_sides` lines 295-320
- `snapshot-multiple-regions`: `lib/tests/test_conflicts.rs::test_materialize_parse_roundtrip` lines 483-580
- `snapshot-long-markers-and-marker-like-content`: `lib/tests/test_conflicts.rs::test_update_conflict_from_content_with_long_markers` lines 1948-2107
- `snapshot-missing-final-newlines`: `lib/tests/test_conflicts.rs::test_update_conflict_from_content_no_eol` lines 2164-2187
- `snapshot-crlf`: `lib/src/conflicts.rs::test_materialize_conflict` lines 1335-1408
- `snapshot-custom-labels`: `lib/tests/test_conflicts.rs::test_materialize_conflict_with_labels` lines 854-908
- `snapshot-empty-term-or-deletion`: `lib/tests/test_conflicts.rs::test_materialize_conflict_no_newlines_at_eof` lines 645-693
- `default-diff-style-2-sided`: `lib/tests/test_conflicts.rs::test_materialize_conflict_basic` lines 89-106
- `git-diff3-style-2-sided`: `lib/tests/test_conflicts.rs::test_parse_conflict_simple` lines 1104-1141
- `malformed-missing-section-header`: `lib/tests/test_conflicts.rs::test_parse_conflict_snapshot_missing_header` lines 1546-1566
- `malformed-missing-diff`: `lib/tests/test_conflicts.rs::test_parse_conflict_malformed_diff` lines 1521-1543
- `malformed-mixed-header-style`: `lib/tests/test_conflicts.rs::test_parse_conflict_mixed_header_styles` lines 1640-1660
- `wrong-arity`: `lib/tests/test_conflicts.rs::test_parse_conflict_wrong_arity` lines 1450-1472
- `git-too-many-sides`: `lib/tests/test_conflicts.rs::test_parse_conflict_git_too_many_sides` lines 1613-1637
- `diff-whitespace-stripped`: `lib/tests/test_conflicts.rs::test_parse_conflict_diff_stripped_whitespace` lines 1404-1447

The long-marker fixture deliberately uses the library snapshot assertion from
`test_update_conflict_from_content_with_long_markers`; the CLI test is only
corroborating evidence. The multiple-region and empty-term snapshot entries are
small, stable adaptations of the cited upstream bodies to the corpus's
snapshot-only v1 contract. Rust indentation, escape decoding, display-only
`[EOF]` removal, and any label policy are recorded in each case's metadata.
No operation IDs, commit IDs, or other volatile CLI output was copied.

The extracted files are adapted test data, not copied executable Rust code or a
copy of JJ's test harness.

## Attribution and license

Copyright © The JJ contributors. JJ is distributed under the Apache License,
Version 2.0. The complete applicable license text is vendored at
`JJ-LICENSE-APACHE-2.0.txt`; the canonical license URL is
<https://www.apache.org/licenses/LICENSE-2.0>. This attribution applies to the
extracted JJ-derived fixture data. The surrounding corpus metadata and
validator are project-local additions.
