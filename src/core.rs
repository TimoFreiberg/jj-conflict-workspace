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

    #[derive(Debug, Clone, PartialEq)]
    enum MetadataJson {
        Object(Vec<(String, MetadataJson)>),
        Array(Vec<MetadataJson>),
        String(String),
        Number(usize),
        Bool(bool),
        Null,
    }

    struct MetadataParser<'a> {
        bytes: &'a [u8],
        offset: usize,
    }

    impl<'a> MetadataParser<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, offset: 0 }
        }

        fn parse(mut self) -> Result<MetadataJson, String> {
            let value = self.value()?;
            self.whitespace();
            if self.offset != self.bytes.len() {
                return Err(format!("unexpected metadata byte at {}", self.offset));
            }
            Ok(value)
        }

        fn value(&mut self) -> Result<MetadataJson, String> {
            self.whitespace();
            match self.bytes.get(self.offset).copied() {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'\"') => self.string().map(MetadataJson::String),
                Some(b'0'..=b'9') => self.number().map(MetadataJson::Number),
                Some(b't') => self.literal(b"true", MetadataJson::Bool(true)),
                Some(b'f') => self.literal(b"false", MetadataJson::Bool(false)),
                Some(b'n') => self.literal(b"null", MetadataJson::Null),
                _ => Err(format!("invalid metadata value at {}", self.offset)),
            }
        }

        fn object(&mut self) -> Result<MetadataJson, String> {
            self.expect(b'{')?;
            let mut values = Vec::new();
            self.whitespace();
            if self.take(b'}') {
                return Ok(MetadataJson::Object(values));
            }
            loop {
                self.whitespace();
                let key = self.string()?;
                self.whitespace();
                self.expect(b':')?;
                let value = self.value()?;
                values.push((key, value));
                self.whitespace();
                if self.take(b'}') {
                    return Ok(MetadataJson::Object(values));
                }
                self.expect(b',')?;
            }
        }

        fn array(&mut self) -> Result<MetadataJson, String> {
            self.expect(b'[')?;
            let mut values = Vec::new();
            self.whitespace();
            if self.take(b']') {
                return Ok(MetadataJson::Array(values));
            }
            loop {
                values.push(self.value()?);
                self.whitespace();
                if self.take(b']') {
                    return Ok(MetadataJson::Array(values));
                }
                self.expect(b',')?;
            }
        }

        fn string(&mut self) -> Result<String, String> {
            self.expect(b'\"')?;
            let mut bytes = Vec::new();
            while let Some(byte) = self.bytes.get(self.offset).copied() {
                self.offset += 1;
                match byte {
                    b'\"' => {
                        return String::from_utf8(bytes)
                            .map_err(|_| "metadata string is not UTF-8".to_owned());
                    }
                    b'\\' => {
                        let escaped = self
                            .bytes
                            .get(self.offset)
                            .copied()
                            .ok_or_else(|| "truncated metadata escape".to_owned())?;
                        self.offset += 1;
                        match escaped {
                            b'\"' | b'\\' | b'/' => bytes.push(escaped),
                            b'b' => bytes.push(8),
                            b'f' => bytes.push(12),
                            b'n' => bytes.push(b'\n'),
                            b'r' => bytes.push(b'\r'),
                            b't' => bytes.push(b'\t'),
                            b'u' => {
                                let code = self.hex_quad()?;
                                let character = char::from_u32(code as u32)
                                    .ok_or_else(|| "invalid metadata unicode escape".to_owned())?;
                                let mut encoded = [0; 4];
                                bytes.extend_from_slice(
                                    character.encode_utf8(&mut encoded).as_bytes(),
                                );
                            }
                            _ => return Err(format!("invalid metadata escape at {}", self.offset)),
                        }
                    }
                    0..=31 => return Err("unescaped metadata control byte".to_owned()),
                    _ => bytes.push(byte),
                }
            }
            Err("unterminated metadata string".to_owned())
        }

        fn hex_quad(&mut self) -> Result<u16, String> {
            let end = self.offset.saturating_add(4);
            let bytes = self
                .bytes
                .get(self.offset..end)
                .ok_or_else(|| "truncated metadata unicode escape".to_owned())?;
            self.offset = end;
            let mut value = 0u16;
            for byte in bytes {
                let digit = match byte {
                    b'0'..=b'9' => (byte - b'0') as u16,
                    b'a'..=b'f' => (byte - b'a' + 10) as u16,
                    b'A'..=b'F' => (byte - b'A' + 10) as u16,
                    _ => return Err("invalid metadata unicode escape".to_owned()),
                };
                value = value
                    .checked_mul(16)
                    .and_then(|value| value.checked_add(digit))
                    .ok_or_else(|| "invalid metadata unicode escape".to_owned())?;
            }
            Ok(value)
        }

        fn number(&mut self) -> Result<usize, String> {
            let start = self.offset;
            while matches!(self.bytes.get(self.offset), Some(b'0'..=b'9')) {
                self.offset += 1;
            }
            std::str::from_utf8(&self.bytes[start..self.offset])
                .map_err(|_| "invalid metadata number".to_owned())?
                .parse()
                .map_err(|_| "metadata number is out of range".to_owned())
        }

        fn literal(&mut self, literal: &[u8], value: MetadataJson) -> Result<MetadataJson, String> {
            let end = self.offset.saturating_add(literal.len());
            if self.bytes.get(self.offset..end) == Some(literal) {
                self.offset = end;
                Ok(value)
            } else {
                Err(format!("invalid metadata literal at {}", self.offset))
            }
        }

        fn expect(&mut self, expected: u8) -> Result<(), String> {
            if self.take(expected) {
                Ok(())
            } else {
                Err(format!(
                    "expected metadata byte {expected:?} at {}",
                    self.offset
                ))
            }
        }

        fn take(&mut self, expected: u8) -> bool {
            if self.bytes.get(self.offset) == Some(&expected) {
                self.offset += 1;
                true
            } else {
                false
            }
        }

        fn whitespace(&mut self) {
            while self
                .bytes
                .get(self.offset)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                self.offset += 1;
            }
        }
    }

    fn metadata_field<'a>(value: &'a MetadataJson, key: &str) -> &'a MetadataJson {
        match value {
            MetadataJson::Object(fields) => fields
                .iter()
                .find_map(|(field, value)| (field == key).then_some(value))
                .unwrap_or_else(|| panic!("metadata field {key:?} is missing")),
            _ => panic!("metadata value is not an object while looking for {key:?}"),
        }
    }

    fn metadata_string(value: &MetadataJson, key: &str) -> String {
        match metadata_field(value, key) {
            MetadataJson::String(value) => value.clone(),
            other => panic!("metadata field {key:?} is not a string: {other:?}"),
        }
    }

    fn metadata_number(value: &MetadataJson, key: &str) -> usize {
        match metadata_field(value, key) {
            MetadataJson::Number(value) => *value,
            other => panic!("metadata field {key:?} is not a number: {other:?}"),
        }
    }

    fn metadata_bool(value: &MetadataJson, key: &str) -> bool {
        match metadata_field(value, key) {
            MetadataJson::Bool(value) => *value,
            other => panic!("metadata field {key:?} is not a boolean: {other:?}"),
        }
    }

    fn metadata_array<'a>(value: &'a MetadataJson, key: &str) -> &'a [MetadataJson] {
        match metadata_field(value, key) {
            MetadataJson::Array(values) => values,
            other => panic!("metadata field {key:?} is not an array: {other:?}"),
        }
    }

    fn metadata_file(path: &Path) -> MetadataJson {
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        MetadataParser::new(&bytes)
            .parse()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    fn metadata_path(path: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    }

    fn corpus_index() -> MetadataJson {
        metadata_file(&corpus_root().join("index.json"))
    }

    fn corpus_cases() -> Vec<MetadataJson> {
        metadata_array(&corpus_index(), "cases").to_vec()
    }

    fn case_root(index_case: &MetadataJson) -> PathBuf {
        let input_path = metadata_string(index_case, "input_path");
        metadata_path(&input_path)
            .parent()
            .expect("corpus input must have a parent directory")
            .to_owned()
    }

    fn case_metadata(index_case: &MetadataJson) -> MetadataJson {
        metadata_file(&case_root(index_case).join("case.json"))
    }

    fn expected_term_kind(value: &MetadataJson) -> TermKind {
        match metadata_string(value, "kind").as_str() {
            "side" => TermKind::Side,
            "base" => TermKind::Base,
            kind => panic!("unknown corpus term kind {kind:?}"),
        }
    }

    fn assert_supported_metadata_case(index_case: &MetadataJson) {
        let name = metadata_string(index_case, "case_name");
        let root = case_root(index_case);
        let metadata = case_metadata(index_case);
        assert_eq!(
            metadata_string(&metadata, "status"),
            "supported",
            "{name}: status"
        );
        assert_eq!(
            metadata_string(&metadata, "case_name"),
            name,
            "{name}: case name"
        );

        let input_path = metadata_path(&metadata_string(index_case, "input_path"));
        let input = fs::read(&input_path).unwrap_or_else(|error| panic!("{name}: {error}"));
        let document = parse_snapshot(&input).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            document.source.as_ref(),
            input.as_slice(),
            "{name}: source bytes"
        );
        assert_eq!(
            document.regions.len(),
            metadata_number(&metadata, "region_count"),
            "{name}: region count"
        );

        let marker = metadata_field(&metadata, "marker");
        assert_eq!(
            document.marker.outer_marker_width,
            metadata_number(marker, "outer_marker_width"),
            "{name}: outer marker width"
        );
        assert_eq!(
            document.marker.section_marker_width,
            metadata_number(marker, "section_marker_width"),
            "{name}: section marker width"
        );

        let regions = metadata_array(&metadata, "regions");
        assert_eq!(
            regions.len(),
            document.regions.len(),
            "{name}: metadata regions"
        );
        let mut previous_end = 0;
        for (region_index, (region, expected_region)) in
            document.regions.iter().zip(regions.iter()).enumerate()
        {
            assert_eq!(
                metadata_number(expected_region, "region_id"),
                region_index,
                "{name}: region {region_index} metadata id"
            );
            let terms = metadata_array(expected_region, "terms");
            assert_eq!(
                terms.len(),
                metadata_number(expected_region, "term_count"),
                "{name}: region {region_index} metadata term count"
            );
            assert_eq!(
                region.terms.len(),
                terms.len(),
                "{name}: region {region_index} term count"
            );
            assert_eq!(
                &input[previous_end..region.source_range.start],
                &document.source[previous_end..region.source_range.start],
                "{name}: region {region_index} prefix outside bytes"
            );
            previous_end = region.source_range.end;

            for (ordinal, (term, expected_term)) in
                region.terms.iter().zip(terms.iter()).enumerate()
            {
                assert_eq!(
                    term.ordinal,
                    metadata_number(expected_term, "ordinal"),
                    "{name}: region {region_index} term {ordinal} ordinal"
                );
                assert_eq!(
                    term.kind,
                    expected_term_kind(expected_term),
                    "{name}: region {region_index} term {ordinal} kind"
                );
                assert_eq!(
                    term.label,
                    metadata_string(expected_term, "label"),
                    "{name}: region {region_index} term {ordinal} label"
                );
                assert_eq!(
                    term.synthetic_separator_eol_removed,
                    metadata_bool(expected_term, "synthetic_separator_eol"),
                    "{name}: region {region_index} term {ordinal} synthetic separator"
                );
                let artifact = fs::read(metadata_path(&metadata_string(expected_term, "path")))
                    .unwrap_or_else(|error| {
                        panic!("{name}: region {region_index} term {ordinal}: {error}")
                    });
                assert_eq!(
                    &*term.logical_bytes,
                    artifact.as_slice(),
                    "{name}: region {region_index} term {ordinal} logical bytes"
                );
            }
        }
        assert_eq!(
            &input[previous_end..],
            &document.source[previous_end..],
            "{name}: suffix outside bytes"
        );

        let resolved_artifact = metadata_field(metadata_field(&metadata, "artifacts"), "resolved");
        let resolved_path = metadata_string(resolved_artifact, "path");
        let resolved = fs::read(metadata_path(&resolved_path))
            .unwrap_or_else(|error| panic!("{name}: resolved artifact: {error}"));
        assert_eq!(
            materialize_scaffold(&document).unwrap(),
            resolved,
            "{name}: resolved scaffold"
        );
        assert_eq!(
            root.join("input.snapshot"),
            input_path,
            "{name}: corpus root/input path"
        );
    }

    #[test]
    fn metadata_driven_supported_corpus_is_exhaustive() {
        let cases = corpus_cases();
        let supported = cases
            .iter()
            .filter(|case| metadata_string(case, "status") == "supported")
            .collect::<Vec<_>>();
        assert_eq!(
            supported.len(),
            8,
            "index must enumerate all supported corpus cases"
        );
        for case in supported {
            assert_supported_metadata_case(case);
        }
    }

    #[test]
    fn metadata_driven_reference_corpus_is_rejected() {
        let cases = corpus_cases();
        let references = cases
            .iter()
            .filter(|case| metadata_string(case, "status") == "reference")
            .collect::<Vec<_>>();
        assert_eq!(
            references.len(),
            8,
            "index must enumerate all reference corpus cases"
        );
        for index_case in references {
            let name = metadata_string(index_case, "case_name");
            let metadata = case_metadata(index_case);
            let disposition = metadata_string(&metadata, "expected_disposition");
            let input = fs::read(metadata_path(&metadata_string(index_case, "input_path")))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let error = match parse_snapshot(&input) {
                Ok(_) => panic!("{name}: reference input was accepted"),
                Err(error) => error,
            };
            let rendered = error.to_string();
            assert!(
                !rendered.is_empty(),
                "{name}: rejection had no useful message"
            );
            match disposition.as_str() {
                "unsupported_style" => assert!(
                    matches!(
                        error,
                        DomainError::UnsupportedStyle { .. } | DomainError::InvalidInput { .. }
                    ),
                    "{name}: expected unsupported-style or malformed rejection, got {error:?}"
                ),
                "malformed" => assert!(
                    matches!(
                        error,
                        DomainError::InvalidInput { .. } | DomainError::UnsupportedStyle { .. }
                    ),
                    "{name}: expected malformed/reference rejection, got {error:?}"
                ),
                "wrong_arity" | "unsupported_or_malformed" | "reference_only" => assert!(
                    matches!(
                        error,
                        DomainError::InvalidInput { .. } | DomainError::UnsupportedStyle { .. }
                    ),
                    "{name}: expected rejection, got {error:?}"
                ),
                other => panic!("{name}: unknown expected disposition {other:?}"),
            }
            match error {
                DomainError::InvalidInput {
                    region_index,
                    byte_offset,
                    ..
                } => assert!(
                    region_index.is_some() || byte_offset.is_some(),
                    "{name}: malformed rejection lacked region/offset context: {rendered}"
                ),
                DomainError::UnsupportedStyle { .. } => {
                    assert!(
                        rendered.contains("unsupported snapshot style"),
                        "{name}: {rendered}"
                    );
                }
                _ => unreachable!("the category assertion above admits only parser errors"),
            }
        }
    }

    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
            let expected = legacy_expected_case(case);
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

    #[allow(dead_code)]
    #[derive(Clone, Copy)]
    struct ExpectedTerm {
        kind: TermKind,
        label: &'static str,
        synthetic: bool,
    }

    #[allow(dead_code)]
    fn legacy_expected_case(name: &str) -> Vec<Vec<ExpectedTerm>> {
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
            "snapshot-multiple-regions" | "snapshot-long-markers-and-marker-like-content" => vec![
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

    #[test]
    #[allow(dead_code)]
    fn supported_corpus_parse_and_scaffold_legacy_expectations() {
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
            let expected = legacy_expected_case(case);
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
            assert_eq!(
                materialize_scaffold(&document).unwrap(),
                fs::read(root.join("resolved")).unwrap(),
                "{case}: resolved"
            );
        }
    }

    #[derive(Clone, Debug)]
    struct GeneratedCase {
        input: Vec<u8>,
        expected_terms: Vec<Vec<(TermKind, String, Vec<u8>)>>,
        expected_resolved: Vec<u8>,
        region_ranges: Vec<(usize, usize)>,
        close_offsets: Vec<usize>,
        first_header_ranges: Vec<(usize, usize)>,
        base_header_offsets: Vec<usize>,
        width: usize,
    }

    #[derive(Clone, Copy)]
    struct DeterministicRng(u64);

    impl DeterministicRng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u8 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 32) as u8
        }

        fn range(&mut self, upper: usize) -> usize {
            usize::from(self.next()) % upper
        }
    }

    fn generated_case(case_index: usize) -> GeneratedCase {
        let mut rng = DeterministicRng::new(0x5eed_0000_u64 + case_index as u64);
        let width = if case_index == 0 {
            16
        } else {
            7 + rng.range(10)
        };
        let region_count = if case_index == 0 { 2 } else { 1 + rng.range(3) };
        let term_count = if case_index == 0 { 5 } else { 2 + rng.range(6) };
        let crlf = case_index % 2 == 0;
        let synthetic_last = case_index % 3 == 0;
        let eol: &[u8] = if crlf { b"\r\n" } else { b"\n" };
        let mut input = Vec::new();
        let mut expected_resolved = Vec::new();
        let mut expected_terms = Vec::new();
        let mut region_ranges = Vec::new();
        let mut close_offsets = Vec::new();
        let mut first_header_ranges = Vec::new();
        let mut base_header_offsets = Vec::new();

        for region_index in 0..region_count {
            input.extend_from_slice(format!("outside-before-{region_index}").as_bytes());
            input.extend_from_slice(eol);
            expected_resolved
                .extend_from_slice(format!("outside-before-{region_index}").as_bytes());
            expected_resolved.extend_from_slice(eol);

            let region_start = input.len();
            input.extend(std::iter::repeat_n(b'<', width));
            input.extend_from_slice(b" opening");
            input.extend_from_slice(eol);
            let synthetic = synthetic_last && region_index + 1 == region_count;
            let mut region_terms = Vec::new();
            for ordinal in 0..term_count {
                let kind = if ordinal % 2 == 1 {
                    TermKind::Base
                } else {
                    TermKind::Side
                };
                let label = format!(" generated-{region_index}-{ordinal}");
                let header_start = input.len();
                input.extend(std::iter::repeat_n(
                    match kind {
                        TermKind::Side => b'+',
                        TermKind::Base => b'-',
                    },
                    width,
                ));
                input.extend_from_slice(label.as_bytes());
                input.extend_from_slice(eol);
                if ordinal == 0 {
                    first_header_ranges.push((header_start, input.len()));
                }
                if kind == TermKind::Base && ordinal == 1 {
                    base_header_offsets.push(header_start);
                }

                let mut logical = Vec::new();
                if region_index == 0 && ordinal == 0 {
                    logical.extend(std::iter::repeat_n(b'<', width - 1));
                    logical.extend_from_slice(b" marker-like\n");
                }
                for _ in 0..(rng.range(14)) {
                    logical.push(match rng.range(11) {
                        0 => 0,
                        1 => 1,
                        2 => b'a',
                        3 => b'Z',
                        4 => b' ',
                        5 => b'\n',
                        6 => b'\r',
                        7 => 0x7f,
                        8 => 0x80,
                        9 => 0xfe,
                        _ => 0xff,
                    });
                }
                // With LF section separators, a terminal payload CR would be
                // indistinguishable from the CR in a CRLF terminator. Keep the
                // generated binary coverage while making the boundary explicit.
                if eol == b"\n" && logical.last() == Some(&b'\r') {
                    logical.push(0);
                }
                if !synthetic && !logical.is_empty() && !logical.ends_with(eol) {
                    logical.extend_from_slice(eol);
                }
                input.extend_from_slice(&logical);
                if synthetic || (!logical.is_empty() && !logical.ends_with(eol)) {
                    input.extend_from_slice(eol);
                }
                region_terms.push((kind, label, logical));
            }

            let close_offset = input.len();
            close_offsets.push(close_offset);
            input.extend(std::iter::repeat_n(b'>', width));
            input.extend_from_slice(b" closing");
            if !synthetic {
                input.extend_from_slice(eol);
            }
            region_ranges.push((region_start, input.len()));
            expected_resolved.extend_from_slice(&region_terms[0].2);
            expected_terms.push(region_terms);

            if !synthetic {
                input.extend_from_slice(format!("outside-after-{region_index}").as_bytes());
                input.extend_from_slice(eol);
                expected_resolved
                    .extend_from_slice(format!("outside-after-{region_index}").as_bytes());
                expected_resolved.extend_from_slice(eol);
            }
        }

        GeneratedCase {
            input,
            expected_terms,
            expected_resolved,
            region_ranges,
            close_offsets,
            first_header_ranges,
            base_header_offsets,
            width,
        }
    }

    fn assert_generated_case(case_index: usize, generated: &GeneratedCase) {
        let document = parse_snapshot(&generated.input)
            .unwrap_or_else(|error| panic!("generated case {case_index} should parse: {error}"));
        assert_eq!(
            document.regions.len(),
            generated.expected_terms.len(),
            "generated case {case_index}: regions"
        );
        assert_eq!(
            document.marker.outer_marker_width, generated.width,
            "generated case {case_index}: width"
        );
        for (region_index, (region, expected_terms)) in document
            .regions
            .iter()
            .zip(&generated.expected_terms)
            .enumerate()
        {
            assert_eq!(
                region.source_range.start, generated.region_ranges[region_index].0,
                "generated case {case_index}: region {region_index} start"
            );
            assert_eq!(
                region.source_range.end, generated.region_ranges[region_index].1,
                "generated case {case_index}: region {region_index} end"
            );
            assert_eq!(
                region.terms.len(),
                expected_terms.len(),
                "generated case {case_index}: region {region_index} term count"
            );
            for (ordinal, (term, (kind, label, logical))) in
                region.terms.iter().zip(expected_terms).enumerate()
            {
                assert_eq!(
                    term.ordinal, ordinal,
                    "generated case {case_index}: region {region_index} term {ordinal} ordinal"
                );
                assert_eq!(
                    term.kind, *kind,
                    "generated case {case_index}: region {region_index} term {ordinal} kind"
                );
                assert_eq!(
                    &term.label, label,
                    "generated case {case_index}: region {region_index} term {ordinal} label"
                );
                assert_eq!(
                    &*term.logical_bytes, logical,
                    "generated case {case_index}: region {region_index} term {ordinal} bytes"
                );
                assert_eq!(
                    term.synthetic_separator_eol_removed,
                    region_index + 1 == generated.expected_terms.len() && case_index % 3 == 0,
                    "generated case {case_index}: region {region_index} term {ordinal} synthetic"
                );
            }
        }
        assert_eq!(
            materialize_scaffold(&document).unwrap(),
            generated.expected_resolved,
            "generated case {case_index}: scaffold"
        );
        let mut previous_end = 0;
        for (region_index, region) in document.regions.iter().enumerate() {
            assert_eq!(
                &generated.input[previous_end..region.source_range.start],
                &document.source[previous_end..region.source_range.start],
                "generated case {case_index}: region {region_index} prefix"
            );
            previous_end = region.source_range.end;
        }
        assert_eq!(
            &generated.input[previous_end..],
            &document.source[previous_end..],
            "generated case {case_index}: suffix"
        );
    }

    fn assert_generated_rejected(case_index: usize, mutation: &str, input: &[u8]) {
        assert!(
            parse_snapshot(input).is_err(),
            "generated case {case_index} mutation {mutation} was accepted"
        );
    }

    #[test]
    fn deterministic_generated_snapshot_properties_cover_arbitrary_bytes_and_structure() {
        for case_index in 0..96 {
            let generated = generated_case(case_index);
            assert_generated_case(case_index, &generated);

            for (region_index, &close_offset) in generated.close_offsets.iter().enumerate() {
                let mut truncated = generated.input.clone();
                truncated.remove(close_offset + generated.width - 1);
                assert_generated_rejected(
                    case_index,
                    &format!("truncate close region {region_index}"),
                    &truncated,
                );

                let mut widened = generated.input.clone();
                widened.insert(close_offset + generated.width, b'>');
                assert_generated_rejected(
                    case_index,
                    &format!("change close width region {region_index}"),
                    &widened,
                );
            }

            for (region_index, &(header_start, header_end)) in
                generated.first_header_ranges.iter().enumerate()
            {
                let mut missing_header = generated.input.clone();
                missing_header.drain(header_start..header_end);
                assert_generated_rejected(
                    case_index,
                    &format!("remove first section header region {region_index}"),
                    &missing_header,
                );
            }

            for (region_index, &base_offset) in generated.base_header_offsets.iter().enumerate() {
                let mut mixed_style = generated.input.clone();
                for byte in &mut mixed_style[base_offset..base_offset + generated.width] {
                    *byte = b'|';
                }
                assert_generated_rejected(
                    case_index,
                    &format!("mixed structural header region {region_index}"),
                    &mixed_style,
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
