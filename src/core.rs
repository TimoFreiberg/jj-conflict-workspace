//! Pure byte-oriented algorithm boundaries.

use crate::domain::{
    ApplyPlan, ApplyValidationRequest, ByteRange, ConflictRegion, MIN_MARKER_WIDTH, ParsedDocument,
    SnapshotMarker, SnapshotStyle, Term, TermKind,
};
use crate::error::DomainError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Line {
    /// The complete line, including its EOL when one is present.
    full: ByteRange,
    /// The line content, excluding LF and an immediately preceding CR.
    content: ByteRange,
    eol_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MarkerRun {
    width: usize,
    after_run: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawSection {
    kind: TermKind,
    label: ByteRange,
    payload_start: usize,
    payload_end: usize,
    header_offset: usize,
}

/// Scan lines without converting the source to text.
///
/// `full` always retains the original EOL bytes. `content` is the exact range
/// used for marker and label recognition, so LF, CRLF, and a missing final EOL
/// remain distinguishable. A final LF does not manufacture an additional empty
/// line; an actual empty line is represented by a line whose content range is
/// empty.
fn scan_lines(input: &[u8]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0;

    while start < input.len() {
        let mut cursor = start;
        while cursor < input.len() && input[cursor] != b'\n' {
            cursor += 1;
        }

        let (full_end, content_end, eol_len) = if cursor < input.len() {
            let content_end = if cursor > start && input[cursor - 1] == b'\r' {
                cursor - 1
            } else {
                cursor
            };
            (cursor + 1, content_end, cursor + 1 - content_end)
        } else {
            (input.len(), input.len(), 0)
        };

        lines.push(Line {
            full: ByteRange {
                start,
                end: full_end,
            },
            content: ByteRange {
                start,
                end: content_end,
            },
            eol_len,
        });
        start = full_end;
    }

    lines
}

fn marker_run(input: &[u8], line: Line, marker: u8) -> Option<MarkerRun> {
    let start = line.content.start;
    if start >= line.content.end || input[start] != marker {
        return None;
    }

    let mut after_run = start + 1;
    while after_run < line.content.end && input[after_run] == marker {
        after_run += 1;
    }
    Some(MarkerRun {
        width: after_run - start,
        after_run,
    })
}

fn input_error(
    message: impl Into<String>,
    region_index: Option<usize>,
    byte_offset: Option<usize>,
) -> DomainError {
    DomainError::InvalidInput {
        message: message.into(),
        region_index,
        byte_offset,
        range: None,
    }
}

fn unsupported_style(input: &[u8], line: Line, width: usize) -> Option<&'static str> {
    let families = [
        (b'%', "Diff"),
        (b'\\', "Diff"),
        (b'|', "Git/diff3"),
        (b'=', "Git"),
    ];
    families.iter().find_map(|(marker, style)| {
        marker_run(input, line, *marker)
            .filter(|run| run.width >= width)
            .map(|_| *style)
    })
}

fn section_header(
    input: &[u8],
    line: Line,
    width: usize,
    region_index: usize,
) -> Result<Option<RawSection>, DomainError> {
    for (marker, kind) in [(b'+', TermKind::Side), (b'-', TermKind::Base)] {
        if let Some(run) = marker_run(input, line, marker) {
            if run.width > width {
                return Err(input_error(
                    format!(
                        "section marker run has width {}; expected exactly {width}",
                        run.width
                    ),
                    Some(region_index),
                    Some(line.full.start),
                ));
            }
            if run.width == width {
                return Ok(Some(RawSection {
                    kind,
                    label: ByteRange {
                        start: run.after_run,
                        end: line.content.end,
                    },
                    payload_start: line.full.end,
                    payload_end: 0,
                    header_offset: line.full.start,
                }));
            }
        }
    }
    Ok(None)
}

fn first_section(
    input: &[u8],
    line: Option<Line>,
    width: usize,
    region_index: usize,
    offset_if_missing: usize,
) -> Result<RawSection, DomainError> {
    let line = line.ok_or_else(|| {
        input_error(
            "conflict opening must be followed immediately by a side section header",
            Some(region_index),
            Some(offset_if_missing),
        )
    })?;

    if let Some(style) = unsupported_style(input, line, width) {
        return Err(DomainError::UnsupportedStyle {
            style: style.into(),
        });
    }
    if let Some(run) = marker_run(input, line, b'<') {
        if run.width >= width {
            return Err(input_error(
                "nested opening marker before the current conflict closed",
                Some(region_index),
                Some(line.full.start),
            ));
        }
    }
    if let Some(run) = marker_run(input, line, b'>') {
        if run.width >= width {
            return Err(input_error(
                "closing marker appeared before the first side section",
                Some(region_index),
                Some(line.full.start),
            ));
        }
    }
    if let Some(run) = marker_run(input, line, b'-') {
        if run.width >= width {
            return Err(input_error(
                "the first conflict section must be a side, not a base",
                Some(region_index),
                Some(line.full.start),
            ));
        }
    }
    if let Some(section) = section_header(input, line, width, region_index)? {
        if section.kind == TermKind::Side {
            return Ok(section);
        }
    }

    Err(input_error(
        "conflict opening must be followed immediately by a same-width side section header",
        Some(region_index),
        Some(line.full.start),
    ))
}

fn build_term(
    input: &[u8],
    section: RawSection,
    synthetic_separator: bool,
    ordinal: usize,
    region_index: usize,
) -> Result<Term, DomainError> {
    if section.payload_start > section.payload_end || section.payload_end > input.len() {
        return Err(input_error(
            "section payload range is outside the source bytes",
            Some(region_index),
            Some(section.header_offset),
        ));
    }
    if !section.label.within_source(input.len()) || section.label.start > section.label.end {
        return Err(input_error(
            "section label range is outside the source bytes",
            Some(region_index),
            Some(section.header_offset),
        ));
    }

    let mut logical_end = section.payload_end;
    if synthetic_separator && logical_end > section.payload_start {
        if logical_end >= 2 && input.get(logical_end - 2..logical_end) == Some(b"\r\n") {
            logical_end -= 2;
        } else if logical_end >= 1 && input.get(logical_end - 1..logical_end) == Some(b"\n") {
            logical_end -= 1;
        }
    }

    let logical_bytes = input[section.payload_start..logical_end]
        .to_vec()
        .into_boxed_slice();
    Term::from_label_bytes(
        ordinal,
        section.kind,
        &input[section.label.start..section.label.end],
        logical_bytes,
        synthetic_separator,
        Some(region_index),
        section.header_offset,
    )
}

fn parse_region(
    input: &[u8],
    lines: &[Line],
    opening_index: usize,
    width: usize,
    region_index: usize,
) -> Result<(ConflictRegion, usize), DomainError> {
    let opening = lines[opening_index];
    let first = first_section(
        input,
        lines.get(opening_index + 1).copied(),
        width,
        region_index,
        opening.full.end,
    )?;
    let mut sections = vec![first];
    let mut line_index = opening_index + 2;

    while let Some(&line) = lines.get(line_index) {
        if let Some(style) = unsupported_style(input, line, width) {
            return Err(DomainError::UnsupportedStyle {
                style: style.into(),
            });
        }

        if let Some(run) = marker_run(input, line, b'<') {
            if run.width >= width {
                return Err(input_error(
                    "nested opening marker before the current conflict closed",
                    Some(region_index),
                    Some(line.full.start),
                ));
            }
        }

        if let Some(run) = marker_run(input, line, b'>') {
            if run.width > width {
                return Err(input_error(
                    format!(
                        "closing marker run has width {}; expected exactly {width}",
                        run.width
                    ),
                    Some(region_index),
                    Some(line.full.start),
                ));
            }
            if run.width == width {
                if let Some(section) = sections.last_mut() {
                    section.payload_end = line.full.start;
                }
                if sections.len() < 2 {
                    return Err(input_error(
                        "a conflict region must contain at least two sections",
                        Some(region_index),
                        Some(line.full.start),
                    ));
                }

                let synthetic_separator = line.eol_len == 0;
                let mut terms = Vec::with_capacity(sections.len());
                for (ordinal, section) in sections.into_iter().enumerate() {
                    terms.push(build_term(
                        input,
                        section,
                        synthetic_separator,
                        ordinal,
                        region_index,
                    )?);
                }
                let marker = SnapshotMarker::new(SnapshotStyle::Snapshot, width, width)?;
                let source_range = ByteRange::new(opening.full.start, line.full.end)?;
                let region = ConflictRegion::new(source_range, marker, terms)?;
                return Ok((region, line_index + 1));
            }
        }

        if let Some(section) = section_header(input, line, width, region_index)? {
            if let Some(previous) = sections.last_mut() {
                previous.payload_end = line.full.start;
            }
            sections.push(section);
        }
        line_index += 1;
    }

    Err(input_error(
        "unterminated conflict region; no same-width closing marker was found",
        Some(region_index),
        Some(input.len()),
    ))
}

/// Parse the v1 snapshot conflict grammar from source bytes.
///
/// The parser recognizes markers only at line-content starts and computes each
/// term from source ranges. It accepts arbitrary section arity and repeated
/// bases; it never converts arbitrary source bytes through a lossy text API.
pub fn parse_snapshot(input: &[u8]) -> Result<ParsedDocument, DomainError> {
    let lines = scan_lines(input);
    let mut regions = Vec::new();
    let mut document_width = None;
    let mut line_index = 0;

    while let Some(&line) = lines.get(line_index) {
        if let Some(run) = marker_run(input, line, b'<') {
            if run.width < MIN_MARKER_WIDTH {
                return Err(input_error(
                    format!(
                        "opening marker run has width {}; minimum supported width is {MIN_MARKER_WIDTH}",
                        run.width
                    ),
                    None,
                    Some(line.full.start),
                ));
            }

            let width = match document_width {
                Some(expected) if expected != run.width => {
                    return Err(input_error(
                        format!(
                            "opening marker run has width {}; expected document width {expected}",
                            run.width
                        ),
                        Some(regions.len()),
                        Some(line.full.start),
                    ));
                }
                Some(expected) => expected,
                None => {
                    document_width = Some(run.width);
                    run.width
                }
            };
            let (region, next_line) =
                parse_region(input, &lines, line_index, width, regions.len())?;
            regions.push(region);
            line_index = next_line;
        } else {
            line_index += 1;
        }
    }

    let width = document_width
        .ok_or_else(|| input_error("no snapshot conflict opening marker was found", None, None))?;
    let marker = SnapshotMarker::new(SnapshotStyle::Snapshot, width, width)?;
    ParsedDocument::new(input.to_vec(), regions, marker)
}

/// Materialize a structural scaffold by copying every outside byte and
/// replacing each validated region with its first logical term. The first term
/// is only a structural seed, not a semantic merge decision.
pub fn materialize_scaffold(document: &ParsedDocument) -> Result<Vec<u8>, DomainError> {
    document.validate()?;

    let mut output = Vec::new();
    let mut cursor = 0;
    for (region_index, region) in document.regions.iter().enumerate() {
        let range = region.source_range;
        let outside = document.source.get(cursor..range.start).ok_or_else(|| {
            input_error(
                "region gap is outside the source bytes",
                Some(region_index),
                Some(range.start),
            )
        })?;
        output.extend_from_slice(outside);
        let first_term = region.terms.first().ok_or_else(|| {
            input_error(
                "conflict region has no usable first term",
                Some(region_index),
                Some(range.start),
            )
        })?;
        output.extend_from_slice(&first_term.logical_bytes);
        cursor = range.end;
    }
    let suffix = document.source.get(cursor..).ok_or_else(|| {
        input_error(
            "source suffix is outside the source bytes",
            None,
            Some(cursor),
        )
    })?;
    output.extend_from_slice(suffix);
    Ok(output)
}

/// Validate a resolved file and produce an original-coordinate apply plan.
/// Byte comparison and diff policy belong to later tasks.
pub fn validate_apply(_request: ApplyValidationRequest<'_>) -> Result<ApplyPlan, DomainError> {
    Err(DomainError::NotImplemented {
        operation: "validate_apply",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn snapshot(width: usize, sections: &[(&str, TermKind, &[u8])], close_eol: bool) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend(std::iter::repeat(b'<').take(width));
        output.extend_from_slice(b" opening\n");
        for (label, kind, payload) in sections {
            output.extend(
                std::iter::repeat(match kind {
                    TermKind::Side => b'+',
                    TermKind::Base => b'-',
                })
                .take(width),
            );
            output.extend_from_slice(label.as_bytes());
            output.push(b'\n');
            output.extend_from_slice(payload);
        }
        output.extend(std::iter::repeat(b'>').take(width));
        output.extend_from_slice(b" closing");
        if close_eol {
            output.push(b'\n');
        }
        output
    }

    #[test]
    fn line_scanner_preserves_eol_forms() {
        let lines = scan_lines(b"a\r\n\nb\nc");
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].full, ByteRange { start: 0, end: 3 });
        assert_eq!(lines[0].content, ByteRange { start: 0, end: 1 });
        assert_eq!(lines[0].eol_len, 2);
        assert_eq!(lines[1].full, ByteRange { start: 3, end: 4 });
        assert!(lines[1].content.is_empty());
        assert_eq!(lines[1].eol_len, 1);
        assert_eq!(lines[2].full, ByteRange { start: 4, end: 6 });
        assert_eq!(lines[2].eol_len, 1);
        assert_eq!(lines[3].full, ByteRange { start: 6, end: 7 });
        assert_eq!(lines[3].eol_len, 0);
    }

    #[test]
    fn parses_arbitrary_arity_and_repeated_bases() {
        let input = snapshot(
            9,
            &[
                (" side 1", TermKind::Side, b"one\n"),
                (" base 1", TermKind::Base, b"two\n"),
                (" base 2", TermKind::Base, b"three\n"),
                (" side 2", TermKind::Side, b"four\n"),
            ],
            true,
        );
        let document = parse_snapshot(&input).unwrap();
        assert_eq!(document.marker.outer_marker_width, 9);
        assert_eq!(document.regions[0].terms.len(), 4);
        assert_eq!(document.regions[0].terms[1].kind, TermKind::Base);
        assert_eq!(document.regions[0].terms[2].kind, TermKind::Base);
    }

    #[test]
    fn synthetic_separator_removes_one_exact_eol_per_term() {
        let input = snapshot(
            7,
            &[
                (" side", TermKind::Side, b"one\r\n"),
                (" base", TermKind::Base, b"two\n"),
                (" empty", TermKind::Side, b""),
            ],
            false,
        );
        let document = parse_snapshot(&input).unwrap();
        let terms = &document.regions[0].terms;
        assert_eq!(&*terms[0].logical_bytes, b"one");
        assert_eq!(&*terms[1].logical_bytes, b"two");
        assert!(terms[2].logical_bytes.is_empty());
        assert!(
            terms
                .iter()
                .all(|term| term.synthetic_separator_eol_removed)
        );
    }

    #[test]
    fn preserves_short_marker_like_payload() {
        let input = b"<<<<<<<<<<<<<<<< opening\n++++++++++++++++ side\n<<<< payload\n---------------- base\nline\n++++++++++++++++ side2\n>>>>>>> payload\n>>>>>>>>>>>>>>>> closing\n";
        let document = parse_snapshot(input).unwrap();
        assert_eq!(
            &*document.regions[0].terms[0].logical_bytes,
            b"<<<< payload\n"
        );
        assert_eq!(
            &*document.regions[0].terms[2].logical_bytes,
            b">>>>>>> payload\n"
        );
    }

    #[test]
    fn preserves_regions_and_scaffold_bytes() {
        let input = b"prefix\n<<<<<<< open\n+++++++ left\nleft\n------- base\nbase\n+++++++ right\nright\n>>>>>>> close\nsuffix\n";
        let document = parse_snapshot(input).unwrap();
        assert_eq!(
            materialize_scaffold(&document).unwrap(),
            b"prefix\nleft\nsuffix\n"
        );
        let range = document.regions[0].source_range;
        assert_eq!(&input[..range.start], b"prefix\n");
        assert_eq!(&input[range.end..], b"suffix\n");
    }

    #[test]
    fn rejects_invalid_structure_with_context() {
        let cases = [
            (b"<<<<<<<\n+++++++ side\nx\n>>>>>>\n".as_slice(), 0),
            (
                b"<<<<<<\n+++++++ side\nx\n+++++++ other\ny\n>>>>>>>\n".as_slice(),
                0,
            ),
            (
                b"<<<<<<<\n------- base\nx\n+++++++ side\ny\n>>>>>>>\n".as_slice(),
                0,
            ),
        ];
        for (input, offset) in cases {
            let error = parse_snapshot(input).unwrap_err();
            match error {
                DomainError::InvalidInput {
                    byte_offset: Some(actual),
                    ..
                } => assert!(actual >= offset),
                DomainError::UnsupportedStyle { .. } => {}
                other => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(matches!(
            parse_snapshot(b"ordinary bytes"),
            Err(DomainError::InvalidInput { .. })
        ));
    }

    #[test]
    fn rejects_unsupported_reference_families() {
        let input = b"<<<<<<<\n+++++++ side\nx\n%%%%%%% diff\ny\n------- base\nz\n>>>>>>>\n";
        assert!(matches!(
            parse_snapshot(input),
            Err(DomainError::UnsupportedStyle { .. })
        ));
        let input = b"<<<<<<<\n+++++++ side\nx\n||||||| base\ny\n------- base\nz\n>>>>>>>\n";
        assert!(matches!(
            parse_snapshot(input),
            Err(DomainError::UnsupportedStyle { .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf8_section_labels_with_context() {
        let input = b"<<<<<<<\n+++++++\xff\npayload\n------- base\nbase\n>>>>>>>\n";
        assert!(matches!(
            parse_snapshot(input),
            Err(DomainError::InvalidInput {
                region_index: Some(0),
                byte_offset: Some(8),
                ..
            })
        ));
    }

    #[test]
    fn marker_family_width_boundaries_are_explicit() {
        let short_payload = [b'%', b'\\', b'|', b'=', b'<', b'+', b'-', b'>'];
        let mut input = Vec::new();
        input.extend_from_slice(b"<<<<<<<< opening\n++++++++ side\n");
        for marker in short_payload {
            input.extend(std::iter::repeat(marker).take(7));
            input.extend_from_slice(b" payload\n");
        }
        input.extend_from_slice(b"-------- base\nbase\n>>>>>>>> close\n");
        let document = parse_snapshot(&input).unwrap();
        let payload = &*document.regions[0].terms[0].logical_bytes;
        for marker in short_payload {
            assert!(payload.windows(9).any(|window| window[0] == marker));
        }

        for marker in [b'%', b'\\', b'|', b'='] {
            let mut candidate = Vec::new();
            candidate.extend_from_slice(b"<<<<<<<< opening\n++++++++ side\n");
            candidate.extend(std::iter::repeat(marker).take(8));
            candidate.extend_from_slice(b" unsupported\n-------- base\nbase\n>>>>>>>> close\n");
            assert!(matches!(
                parse_snapshot(&candidate),
                Err(DomainError::UnsupportedStyle { .. })
            ));
        }
    }

    #[test]
    fn malformed_bytes_never_panic() {
        let inputs: &[&[u8]] = &[
            b"",
            b"<",
            b"<<<<<<<",
            b"<<<<<<<\n",
            b"<<<<<<<\n+++++++\n",
            b"<<<<<<<\n+++++++\xff\nx\n-------\ny\n>>>>>>>",
            b"<<<<<<<\n+++++++\n\n>>>>>>>",
        ];
        for input in inputs {
            let result = std::panic::catch_unwind(|| parse_snapshot(input));
            assert!(result.is_ok(), "parser panicked for {input:?}");
            assert!(
                result.unwrap().is_err(),
                "malformed input was accepted: {input:?}"
            );
        }
    }

    #[test]
    fn materializer_rejects_malformed_public_documents() {
        let marker = SnapshotMarker::default();
        let term = Term::new(0, TermKind::Side, "side", b"x".to_vec(), false).unwrap();
        let region = ConflictRegion {
            source_range: ByteRange { start: 0, end: 20 },
            marker,
            terms: vec![term.clone()],
        };
        let document = ParsedDocument {
            source: b"x".to_vec().into_boxed_slice(),
            regions: vec![region],
            marker,
        };
        assert!(materialize_scaffold(&document).is_err());

        let empty_terms = ConflictRegion {
            source_range: ByteRange { start: 0, end: 1 },
            marker,
            terms: Vec::new(),
        };
        let document = ParsedDocument {
            source: b"x".to_vec().into_boxed_slice(),
            regions: vec![empty_terms],
            marker,
        };
        assert!(materialize_scaffold(&document).is_err());

        let overlapping = ParsedDocument {
            source: b"xyz".to_vec().into_boxed_slice(),
            regions: vec![
                ConflictRegion {
                    source_range: ByteRange { start: 0, end: 2 },
                    marker,
                    terms: vec![term.clone()],
                },
                ConflictRegion {
                    source_range: ByteRange { start: 1, end: 3 },
                    marker,
                    terms: vec![term.clone()],
                },
            ],
            marker,
        };
        assert!(materialize_scaffold(&overlapping).is_err());

        let mismatched_marker = SnapshotMarker::new(SnapshotStyle::Snapshot, 8, 8).unwrap();
        let marker_mismatch = ParsedDocument {
            source: b"xyz".to_vec().into_boxed_slice(),
            regions: vec![ConflictRegion {
                source_range: ByteRange { start: 0, end: 1 },
                marker: mismatched_marker,
                terms: vec![term],
            }],
            marker,
        };
        assert!(materialize_scaffold(&marker_mismatch).is_err());
    }

    #[derive(Clone, Copy)]
    struct ExpectedTerm {
        kind: TermKind,
        label: &'static str,
        synthetic: bool,
    }

    fn expected_case(name: &str) -> Vec<Vec<ExpectedTerm>> {
        let normal = |labels: &[&'static str]| {
            labels
                .iter()
                .enumerate()
                .map(|(index, label)| ExpectedTerm {
                    kind: if index % 2 == 1 {
                        TermKind::Base
                    } else {
                        TermKind::Side
                    },
                    label,
                    synthetic: false,
                })
                .collect::<Vec<_>>()
        };
        match name {
            "snapshot-basic-2-sided" => vec![normal(&[" side #1", " base", " side #2"])],
            "snapshot-3-sided-with-multiple-bases" => vec![vec![
                ExpectedTerm {
                    kind: TermKind::Side,
                    label: " side #1",
                    synthetic: false,
                },
                ExpectedTerm {
                    kind: TermKind::Base,
                    label: " base #1",
                    synthetic: false,
                },
                ExpectedTerm {
                    kind: TermKind::Side,
                    label: " side #2",
                    synthetic: false,
                },
                ExpectedTerm {
                    kind: TermKind::Base,
                    label: " base #2",
                    synthetic: false,
                },
                ExpectedTerm {
                    kind: TermKind::Side,
                    label: " side #3",
                    synthetic: false,
                },
            ]],
            "snapshot-multiple-regions" => vec![
                normal(&[" side #1", " base", " side #2"]),
                normal(&[" side #1", " base", " side #2"]),
            ],
            "snapshot-long-markers-and-marker-like-content" => vec![
                normal(&[" side #1", " base", " side #2"]),
                normal(&[" side #1", " base", " side #2"]),
            ],
            "snapshot-missing-final-newlines" => vec![
                normal(&[" side #1", " base", " side #2"]),
                vec![
                    ExpectedTerm {
                        kind: TermKind::Side,
                        label: " side #1",
                        synthetic: true,
                    },
                    ExpectedTerm {
                        kind: TermKind::Base,
                        label: " base (no terminating newline)",
                        synthetic: true,
                    },
                    ExpectedTerm {
                        kind: TermKind::Side,
                        label: " side #2 (no terminating newline)",
                        synthetic: true,
                    },
                ],
            ],
            "snapshot-crlf" => vec![normal(&[" side #1", " base", " side #2"])],
            "snapshot-custom-labels" => vec![vec![
                ExpectedTerm {
                    kind: TermKind::Side,
                    label: " side 1 conflict label",
                    synthetic: false,
                },
                ExpectedTerm {
                    kind: TermKind::Base,
                    label: " base conflict label",
                    synthetic: false,
                },
                ExpectedTerm {
                    kind: TermKind::Side,
                    label: " side 2 conflict label",
                    synthetic: false,
                },
            ]],
            "snapshot-empty-term-or-deletion" => vec![normal(&[" side #1", " base", " side #2"])],
            _ => panic!("unknown corpus case {name}"),
        }
    }

    fn corpus_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/test-corpus")
    }

    fn expected_case_ranges(name: &str) -> &'static [(usize, usize)] {
        match name {
            "snapshot-basic-2-sided" => &[(14, 156)],
            "snapshot-3-sided-with-multiple-bases" => &[(7, 279)],
            "snapshot-multiple-regions" => &[(0, 156), (163, 320)],
            "snapshot-long-markers-and-marker-like-content" => &[(0, 178), (185, 376)],
            "snapshot-missing-final-newlines" => &[(7, 137), (144, 313)],
            "snapshot-crlf" => &[(2, 117)],
            "snapshot-custom-labels" => &[(0, 162)],
            "snapshot-empty-term-or-deletion" => &[(0, 109)],
            _ => panic!("unknown corpus case {name}"),
        }
    }

    #[test]
    fn supported_corpus_parse_and_scaffold() {
        let cases = [
            "snapshot-basic-2-sided",
            "snapshot-3-sided-with-multiple-bases",
            "snapshot-multiple-regions",
            "snapshot-long-markers-and-marker-like-content",
            "snapshot-missing-final-newlines",
            "snapshot-crlf",
            "snapshot-custom-labels",
            "snapshot-empty-term-or-deletion",
        ];
        for case in cases {
            let expected = expected_case(case);
            let expected_ranges = expected_case_ranges(case);
            let root = corpus_root().join("cases").join(case);
            let input = fs::read(root.join("input.snapshot")).unwrap();
            let document = parse_snapshot(&input).unwrap_or_else(|error| panic!("{case}: {error}"));
            assert_eq!(
                document.regions.len(),
                expected.len(),
                "{case}: region count"
            );
            assert_eq!(
                document.source.as_ref(),
                input.as_slice(),
                "{case}: source copy"
            );
            assert_eq!(
                document.regions.len(),
                expected_ranges.len(),
                "{case}: range count"
            );
            for (region_index, ((region, expected_terms), &(expected_start, expected_end))) in
                document
                    .regions
                    .iter()
                    .zip(expected.iter())
                    .zip(expected_ranges.iter())
                    .enumerate()
            {
                assert_eq!(
                    (region.source_range.start, region.source_range.end),
                    (expected_start, expected_end),
                    "{case}: region {region_index} range"
                );
                assert_eq!(
                    region.terms.len(),
                    expected_terms.len(),
                    "{case}: region {region_index}"
                );
                for (ordinal, (term, expected_term)) in
                    region.terms.iter().zip(expected_terms.iter()).enumerate()
                {
                    assert_eq!(
                        term.ordinal, ordinal,
                        "{case}: region {region_index} ordinal"
                    );
                    assert_eq!(
                        term.kind, expected_term.kind,
                        "{case}: region {region_index} kind"
                    );
                    assert_eq!(
                        term.label, expected_term.label,
                        "{case}: region {region_index} label"
                    );
                    assert_eq!(
                        term.synthetic_separator_eol_removed, expected_term.synthetic,
                        "{case}: region {region_index} synthetic"
                    );
                    let artifact = fs::read(root.join(format!(
                        "regions/region-{region_index:03}/term-{ordinal:03}.term"
                    )))
                    .unwrap();
                    assert_eq!(
                        &*term.logical_bytes,
                        artifact.as_slice(),
                        "{case}: region {region_index} term {ordinal}"
                    );
                }
            }
            let resolved = fs::read(root.join("resolved")).unwrap();
            assert_eq!(
                materialize_scaffold(&document).unwrap(),
                resolved,
                "{case}: resolved"
            );
            let mut previous_end = 0;
            for region in &document.regions {
                assert!(
                    region.source_range.start >= previous_end,
                    "{case}: ranges overlap"
                );
                assert_eq!(
                    &input[previous_end..region.source_range.start],
                    &document.source[previous_end..region.source_range.start]
                );
                previous_end = region.source_range.end;
            }
            assert_eq!(&input[previous_end..], &document.source[previous_end..]);
            if case == "snapshot-long-markers-and-marker-like-content" {
                assert_eq!(document.marker.outer_marker_width, 16);
            } else {
                assert_eq!(document.marker.outer_marker_width, 7);
            }
        }
    }

    #[test]
    fn reference_fixtures_are_rejected_with_expected_categories() {
        let cases = [
            ("default-diff-style-2-sided", true),
            ("git-diff3-style-2-sided", false),
            ("git-too-many-sides", false),
            ("diff-whitespace-stripped", true),
            ("malformed-missing-section-header", false),
            ("malformed-missing-diff", true),
            ("malformed-mixed-header-style", true),
            ("wrong-arity", true),
        ];
        for (name, expect_unsupported) in cases {
            let input = fs::read(
                corpus_root()
                    .join("reference")
                    .join(name)
                    .join("input.snapshot"),
            )
            .unwrap();
            let error = parse_snapshot(&input).unwrap_err();
            if expect_unsupported {
                assert!(
                    matches!(error, DomainError::UnsupportedStyle { .. }),
                    "reference {name} had the wrong error category: {error:?}"
                );
            } else {
                assert!(
                    matches!(
                        error,
                        DomainError::InvalidInput {
                            region_index: Some(0),
                            byte_offset: Some(_),
                            ..
                        }
                    ),
                    "reference {name} lacked region/offset context: {error:?}"
                );
            }
        }
    }

    #[test]
    fn arbitrary_snapshot_arity_is_not_rejected() {
        let input = snapshot(
            7,
            &[
                (" one", TermKind::Side, b"a\n"),
                (" two", TermKind::Side, b"b\n"),
            ],
            true,
        );
        assert_eq!(parse_snapshot(&input).unwrap().regions[0].terms.len(), 2);
    }
}
