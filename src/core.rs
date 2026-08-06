//! Pure byte-oriented algorithm boundaries.

use crate::domain::{
    ApplyPlan, ApplyValidationRequest, ByteRange, ConflictRegion, DiffHunk, MIN_MARKER_WIDTH,
    Manifest, ManifestTerm, ParsedDocument, SnapshotMarker, SnapshotStyle, Term, TermKind,
};
use crate::error::DomainError;
use crate::prepare::sha256;
use std::path::PathBuf;

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
        while cursor < input.len() && input.get(cursor).copied() != Some(b'\n') {
            cursor = cursor.saturating_add(1);
        }

        let (full_end, content_end, eol_len) = if cursor < input.len() {
            let content_end = if cursor > start && input.get(cursor - 1).copied() == Some(b'\r') {
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
    if start >= line.content.end || input.get(start).copied() != Some(marker) {
        return None;
    }

    let mut after_run = start.checked_add(1)?;
    while after_run < line.content.end && input.get(after_run).copied() == Some(marker) {
        after_run = after_run.checked_add(1)?;
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

    let logical_bytes = input
        .get(section.payload_start..logical_end)
        .ok_or_else(|| {
            input_error(
                "section payload range is outside the source bytes",
                Some(region_index),
                Some(section.header_offset),
            )
        })?
        .to_vec()
        .into_boxed_slice();
    let label = input
        .get(section.label.start..section.label.end)
        .ok_or_else(|| {
            input_error(
                "section label range is outside the source bytes",
                Some(region_index),
                Some(section.header_offset),
            )
        })?;
    Term::from_label_bytes(
        ordinal,
        section.kind,
        label,
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
    let opening = lines.get(opening_index).copied().ok_or_else(|| {
        input_error(
            "conflict opening line is outside the scanned source",
            Some(region_index),
            Some(input.len()),
        )
    })?;
    let next_index = opening_index.checked_add(1).ok_or_else(|| {
        input_error(
            "conflict opening line index overflowed",
            Some(region_index),
            Some(opening.full.start),
        )
    })?;
    let first = first_section(
        input,
        lines.get(next_index).copied(),
        width,
        region_index,
        opening.full.end,
    )?;
    let mut sections = vec![first];
    let mut line_index = opening_index.checked_add(2).ok_or_else(|| {
        input_error(
            "conflict line index overflowed",
            Some(region_index),
            Some(opening.full.start),
        )
    })?;

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
                let next_line = line_index.checked_add(1).ok_or_else(|| {
                    input_error(
                        "conflict line index overflowed",
                        Some(region_index),
                        Some(line.full.end),
                    )
                })?;
                return Ok((region, next_line));
            }
        }

        if let Some(section) = section_header(input, line, width, region_index)? {
            if let Some(previous) = sections.last_mut() {
                previous.payload_end = line.full.start;
            }
            sections.push(section);
        }
        line_index = line_index.checked_add(1).ok_or_else(|| {
            input_error(
                "conflict line index overflowed",
                Some(region_index),
                Some(input.len()),
            )
        })?;
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
///
/// A line whose leading run of `<` is shorter than `MIN_MARKER_WIDTH` is
/// ordinary content, not a marker: jj's snapshot-style markers are always at
/// least `MIN_MARKER_WIDTH` characters wide, and real files (Svelte, HTML,
/// JSX, XML) commonly start lines with `<`. Input with no conflict markers at
/// all yields [`DomainError::NoConflictFound`].
pub fn parse_snapshot(input: &[u8]) -> Result<ParsedDocument, DomainError> {
    let lines = scan_lines(input);
    let mut regions = Vec::new();
    let mut document_width = None;
    let mut line_index = 0;

    while let Some(&line) = lines.get(line_index) {
        if let Some(run) = marker_run(input, line, b'<') {
            if run.width < MIN_MARKER_WIDTH {
                // A run shorter than the minimum marker width cannot be a
                // snapshot opening marker; it is ordinary content (e.g. a
                // Svelte/HTML/JSX line starting with '<').
                line_index = line_index.checked_add(1).ok_or_else(|| {
                    input_error("line index overflowed", None, Some(line.full.start))
                })?;
                continue;
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
            line_index = line_index
                .checked_add(1)
                .ok_or_else(|| input_error("line index overflowed", None, Some(input.len())))?;
        }
    }

    // The pure parser has no path to report; the imperative boundary fills in
    // the requested file before rendering this error to the user.
    let width = document_width.ok_or(DomainError::NoConflictFound {
        path: PathBuf::new(),
    })?;
    let marker = SnapshotMarker::new(SnapshotStyle::Snapshot, width, width)?;
    ParsedDocument::new(input.to_vec(), regions, marker)
}

/// Build the exact placeholder seed bytes for a conflict region: one
/// self-describing ASCII line naming the region index and every term artifact
/// path, followed by `trailing_eol` (the closing marker line's EOL mirrored
/// from the source region span: `\r\n`, `\n`, or none).
///
/// The seed is deliberately marker-free: it contains no `< > % + | =` byte,
/// so it can never be mistaken for a native conflict marker at any active
/// width, and no backslash, so it cannot form a JJ continuation label.
pub(crate) fn region_seed(region_index: usize, terms: &[Term], trailing_eol: &[u8]) -> Vec<u8> {
    let mut seed = format!(
        "JCW-UNRESOLVED-CONFLICT-REGION-{region_index:03}: replace this line with the final content for this conflict, or delete the line to drop the content. Terms: "
    );
    for (ordinal, _) in terms.iter().enumerate() {
        if ordinal > 0 {
            seed.push_str(", ");
        }
        seed.push_str(
            &ManifestTerm::generated_artifact_path(region_index, ordinal).to_string_lossy(),
        );
    }
    seed.push_str(&String::from_utf8_lossy(trailing_eol));
    seed.into_bytes()
}

/// The trailing EOL bytes of a conflict region's source span: the closing
/// marker line's EOL, which the seed line mirrors.
pub(crate) fn region_trailing_eol(source: &[u8], range: ByteRange) -> &'static [u8] {
    if range.end <= source.len() && range.end >= 2 && &source[range.end - 2..range.end] == b"\r\n" {
        b"\r\n"
    } else if range.end <= source.len() && range.end >= 1 && source[range.end - 1] == b'\n' {
        b"\n"
    } else {
        b""
    }
}

/// Materialize the unresolved editing canvas by copying every outside byte and
/// replacing each validated region with its JCW placeholder seed line. The
/// seed is not a resolution: `apply` refuses any resolved file that still
/// contains a recorded seed.
pub fn materialize_unresolved(document: &ParsedDocument) -> Result<Vec<u8>, DomainError> {
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
        let trailing_eol = region_trailing_eol(&document.source, range);
        output.extend_from_slice(&region_seed(region_index, &region.terms, trailing_eol));
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
///
/// The manifest and source are the authority for what may change. The resolved
/// bytes are never decoded or normalized: the only bytes that may differ are
/// the original conflict ranges recorded in the manifest.
pub fn validate_apply(request: ApplyValidationRequest<'_>) -> Result<ApplyPlan, DomainError> {
    let manifest = request.manifest;
    manifest.validate()?;

    let actual_digest = sha256(request.original_source);
    if actual_digest != manifest.source_digest.0 {
        return Err(DomainError::StaleSource {
            message: "the current source bytes no longer match the workspace manifest".into(),
            expected: hex_digest(manifest.source_digest.0),
            actual: hex_digest(actual_digest),
        });
    }
    if request.original_source.len() != manifest.source_length {
        return Err(DomainError::StaleSource {
            message: "the current source length no longer matches the workspace manifest".into(),
            expected: manifest.source_length.to_string(),
            actual: request.original_source.len().to_string(),
        });
    }

    reject_resolved_markers(manifest, request.resolved)?;
    let groups = conflict_groups(manifest);
    if groups.is_empty() {
        if request.original_source != request.resolved {
            let offset = first_difference(request.original_source, request.resolved).unwrap_or(0);
            return Err(DomainError::InvalidResolved {
                message: "the manifest contains no conflict region, but the resolved bytes differ"
                    .into(),
                region_index: None,
                range: Some((
                    offset,
                    offset.saturating_add(1).min(request.original_source.len()),
                )),
            });
        }
        return Ok(ApplyPlan::empty());
    }

    let resolved_ranges =
        map_resolved_groups(manifest, request.original_source, request.resolved, &groups)?;
    compare_outside_segments(
        manifest,
        request.original_source,
        request.resolved,
        &groups,
        &resolved_ranges,
    )?;

    // Refuse any resolved file in which a region still contains its recorded
    // JCW placeholder seed bytes. A fully untouched seed is an exact match;
    // a merged group of adjacent regions still contains every untouched
    // region's seed, so partial resolutions are caught as well.
    for (region_index, region) in manifest.regions.iter().enumerate() {
        if region.seed.is_empty() {
            continue;
        }
        let (resolved_start, resolved_end) = resolved_ranges
            .iter()
            .zip(&groups)
            .find(|(_, group)| {
                group.first_region <= region_index && region_index <= group.last_region
            })
            .map(|(&range, _)| range)
            .ok_or_else(|| {
                DomainError::invalid("conflict group is missing for a manifest region")
            })?;
        let haystack = request
            .resolved
            .get(resolved_start..resolved_end)
            .ok_or_else(|| {
                DomainError::invalid("resolved conflict range is outside the resolved bytes")
            })?;
        if let Some(offset) = haystack
            .windows(region.seed.len())
            .position(|window| window == &*region.seed)
        {
            let start = resolved_start
                .checked_add(offset)
                .ok_or_else(|| DomainError::invalid("placeholder offset overflow"))?;
            let end = start
                .checked_add(region.seed.len())
                .ok_or_else(|| DomainError::invalid("placeholder range end overflow"))?;
            let terms = region
                .terms
                .iter()
                .map(|term| term.artifact_path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DomainError::InvalidResolved {
                message: format!(
                    "region {region_index} still contains the JCW unresolved placeholder; replace it with the final content (terms: {terms})"
                ),
                region_index: Some(region_index),
                range: Some((start, end)),
            });
        }
    }

    let mut hunks = Vec::new();
    for (group, &(resolved_start, resolved_end)) in groups.iter().zip(&resolved_ranges) {
        let original_range = ByteRange {
            start: group.start,
            end: group.end,
        };
        let replacement = request
            .resolved
            .get(resolved_start..resolved_end)
            .ok_or_else(|| {
                DomainError::invalid("resolved conflict range is outside the resolved bytes")
            })?;
        let original = request
            .original_source
            .get(original_range.start..original_range.end)
            .ok_or_else(|| DomainError::invalid("conflict range is outside the source bytes"))?;
        if original != replacement {
            hunks.push(DiffHunk::new(
                hunks.len(),
                original_range,
                replacement.to_vec(),
            ));
        }
    }
    ApplyPlan::new(hunks)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConflictGroup {
    start: usize,
    end: usize,
    first_region: usize,
    last_region: usize,
}

fn conflict_groups(manifest: &Manifest) -> Vec<ConflictGroup> {
    let mut groups: Vec<ConflictGroup> = Vec::new();
    for region in &manifest.regions {
        let range = region.source_range;
        if let Some(previous) = groups.last_mut() {
            if range.start == previous.end {
                previous.end = range.end;
                previous.last_region = region.region_index;
                continue;
            }
        }
        groups.push(ConflictGroup {
            start: range.start,
            end: range.end,
            first_region: region.region_index,
            last_region: region.region_index,
        });
    }
    groups
}

fn map_resolved_groups(
    _manifest: &Manifest,
    original: &[u8],
    resolved: &[u8],
    groups: &[ConflictGroup],
) -> Result<Vec<(usize, usize)>, DomainError> {
    let first = groups.first().copied().ok_or_else(|| {
        DomainError::invalid("cannot map resolved bytes without a conflict group")
    })?;
    let last = groups.last().copied().ok_or_else(|| {
        DomainError::invalid("cannot map resolved bytes without a conflict group")
    })?;
    let prefix = original.get(..first.start).ok_or_else(|| {
        input_error(
            "conflict group prefix is outside the source bytes",
            Some(first.first_region),
            Some(first.start),
        )
    })?;
    let suffix = original.get(last.end..).ok_or_else(|| {
        input_error(
            "conflict group suffix is outside the source bytes",
            Some(last.last_region),
            Some(last.end),
        )
    })?;
    if resolved.len() < prefix.len() || resolved.get(..prefix.len()) != Some(prefix) {
        let actual = resolved.get(..prefix.len()).unwrap_or_default();
        let offset = first_difference(prefix, actual).unwrap_or(0);
        return Err(guard_violation(
            first.first_region,
            offset,
            "the source prefix changed or is missing",
        ));
    }
    if resolved.len() < suffix.len() || !resolved.ends_with(suffix) {
        let actual = resolved
            .get(resolved.len().saturating_sub(suffix.len())..)
            .unwrap_or_default();
        let offset = first_difference(suffix, actual).unwrap_or(0);
        return Err(guard_violation(
            last.last_region,
            last.end.saturating_add(offset),
            "the source suffix changed or is missing",
        ));
    }

    let suffix_start = resolved.len().checked_sub(suffix.len()).ok_or_else(|| {
        guard_violation(
            last.last_region,
            last.end,
            "the source suffix changed or is missing",
        )
    })?;
    if prefix.len() > suffix_start {
        return Err(guard_violation(
            first.first_region,
            prefix.len(),
            "resolved conflict boundaries overlap",
        ));
    }

    let mut ranges = vec![(0, 0); groups.len()];
    let first_range = ranges
        .first_mut()
        .ok_or_else(|| DomainError::invalid("cannot map resolved bytes without a range slot"))?;
    first_range.0 = prefix.len();
    if find_group_boundaries(
        0,
        prefix.len(),
        suffix_start,
        original,
        resolved,
        groups,
        &mut ranges,
    ) {
        return Ok(ranges);
    }

    Err(guard_violation(
        first.first_region,
        first.start,
        "bytes between conflict regions changed or are missing",
    ))
}

fn find_group_boundaries(
    group_index: usize,
    replacement_start: usize,
    suffix_start: usize,
    original: &[u8],
    resolved: &[u8],
    groups: &[ConflictGroup],
    ranges: &mut [(usize, usize)],
) -> bool {
    if group_index.checked_add(1) == Some(groups.len()) {
        let Some(range) = ranges.get_mut(group_index) else {
            return false;
        };
        *range = (replacement_start, suffix_start);
        return outside_segments_match(original, resolved, groups, ranges);
    }

    let Some(current) = groups.get(group_index).copied() else {
        return false;
    };
    let Some(next) = groups.get(group_index + 1).copied() else {
        return false;
    };
    let Some(separator) = original.get(current.end..next.start) else {
        return false;
    };
    let search_end = suffix_start.min(resolved.len());
    for candidate in replacement_start..=search_end {
        if resolved.get(candidate..candidate.saturating_add(separator.len())) != Some(separator) {
            continue;
        }
        let Some(range) = ranges.get_mut(group_index) else {
            return false;
        };
        *range = (replacement_start, candidate);
        let Some(next_start) = candidate.checked_add(separator.len()) else {
            return false;
        };
        if find_group_boundaries(
            group_index + 1,
            next_start,
            suffix_start,
            original,
            resolved,
            groups,
            ranges,
        ) {
            return true;
        }
    }
    false
}

fn outside_segments_match(
    original: &[u8],
    resolved: &[u8],
    groups: &[ConflictGroup],
    ranges: &[(usize, usize)],
) -> bool {
    let Some(first) = groups.first().copied() else {
        return false;
    };
    let Some((first_start, _)) = ranges.first().copied() else {
        return false;
    };
    let Some(original_prefix) = original.get(..first.start) else {
        return false;
    };
    if resolved.get(..first_start) != Some(original_prefix) {
        return false;
    }
    for index in 1..groups.len() {
        let (Some(previous), Some(current), Some((_, previous_end)), Some((current_start, _))) = (
            groups.get(index - 1),
            groups.get(index),
            ranges.get(index - 1),
            ranges.get(index),
        ) else {
            return false;
        };
        let Some(original_separator) = original.get(previous.end..current.start) else {
            return false;
        };
        if resolved.get(*previous_end..*current_start) != Some(original_separator) {
            return false;
        }
    }
    let Some(last) = groups.last().copied() else {
        return false;
    };
    let Some((_, last_end)) = ranges.last().copied() else {
        return false;
    };
    let Some(original_suffix) = original.get(last.end..) else {
        return false;
    };
    resolved.get(last_end..) == Some(original_suffix)
}

fn compare_outside_segments(
    manifest: &Manifest,
    original: &[u8],
    resolved: &[u8],
    groups: &[ConflictGroup],
    resolved_ranges: &[(usize, usize)],
) -> Result<(), DomainError> {
    let first = groups.first().copied().ok_or_else(|| {
        DomainError::invalid("cannot compare outside bytes without a conflict group")
    })?;
    let (first_start, _) = resolved_ranges.first().copied().ok_or_else(|| {
        DomainError::invalid("cannot compare outside bytes without a resolved range")
    })?;
    let original_prefix = original.get(..first.start).ok_or_else(|| {
        input_error(
            "conflict group prefix is outside the source bytes",
            Some(first.first_region),
            Some(first.start),
        )
    })?;
    let resolved_prefix = resolved.get(..first_start).ok_or_else(|| {
        DomainError::invalid("resolved prefix range is outside the resolved bytes")
    })?;
    compare_outside(
        first.first_region,
        original_prefix,
        resolved_prefix,
        0,
        "source prefix",
    )?;

    for group_index in 1..groups.len() {
        let previous = groups
            .get(group_index - 1)
            .copied()
            .ok_or_else(|| DomainError::invalid("missing previous conflict group"))?;
        let current = groups
            .get(group_index)
            .copied()
            .ok_or_else(|| DomainError::invalid("missing current conflict group"))?;
        let (_, previous_end) = resolved_ranges
            .get(group_index - 1)
            .copied()
            .ok_or_else(|| DomainError::invalid("missing previous resolved range"))?;
        let (current_start, _) = resolved_ranges
            .get(group_index)
            .copied()
            .ok_or_else(|| DomainError::invalid("missing current resolved range"))?;
        let original_separator = original.get(previous.end..current.start).ok_or_else(|| {
            input_error(
                "bytes between conflict groups are outside the source bytes",
                Some(current.first_region),
                Some(previous.end),
            )
        })?;
        let resolved_separator = resolved.get(previous_end..current_start).ok_or_else(|| {
            DomainError::invalid("resolved separator range is outside the resolved bytes")
        })?;
        compare_outside(
            current.first_region,
            original_separator,
            resolved_separator,
            previous.end,
            "bytes between conflict regions",
        )?;
    }

    let last = groups.last().copied().ok_or_else(|| {
        DomainError::invalid("cannot compare outside bytes without a conflict group")
    })?;
    let (_, last_end) = resolved_ranges
        .last()
        .copied()
        .ok_or_else(|| DomainError::invalid("missing final resolved range"))?;
    let original_suffix = original.get(last.end..).ok_or_else(|| {
        input_error(
            "conflict group suffix is outside the source bytes",
            Some(last.last_region),
            Some(last.end),
        )
    })?;
    let resolved_suffix = resolved.get(last_end..).ok_or_else(|| {
        DomainError::invalid("resolved suffix range is outside the resolved bytes")
    })?;
    compare_outside(
        last.last_region,
        original_suffix,
        resolved_suffix,
        last.end,
        "source suffix",
    )?;
    let _ = manifest;
    Ok(())
}

fn compare_outside(
    region_index: usize,
    expected: &[u8],
    actual: &[u8],
    source_start: usize,
    description: &str,
) -> Result<(), DomainError> {
    if let Some(offset) = first_difference(expected, actual) {
        let start = source_start
            .checked_add(offset.min(expected.len()))
            .unwrap_or(usize::MAX);
        let expected_end = source_start
            .checked_add(expected.len())
            .unwrap_or(usize::MAX);
        let end = if start < expected_end {
            start.saturating_add(1)
        } else {
            start
        };
        return Err(DomainError::GuardViolation {
            region_index,
            start,
            end,
            message: format!(
                "{description} differs at the first offending byte; expected {} bytes, found {}",
                expected.len(),
                actual.len()
            ),
        });
    }
    Ok(())
}

fn guard_violation(region_index: usize, offset: usize, message: &str) -> DomainError {
    DomainError::GuardViolation {
        region_index,
        start: offset,
        end: offset.saturating_add(1),
        message: message.into(),
    }
}

fn first_difference(expected: &[u8], actual: &[u8]) -> Option<usize> {
    let common = expected.len().min(actual.len());
    expected[..common]
        .iter()
        .zip(&actual[..common])
        .position(|(left, right)| left != right)
        .or_else(|| (expected.len() != actual.len()).then_some(common))
}

fn reject_resolved_markers(manifest: &Manifest, resolved: &[u8]) -> Result<(), DomainError> {
    let width = manifest.marker.outer_marker_width;
    let labels: Vec<&[u8]> = manifest
        .regions
        .iter()
        .flat_map(|region| region.terms.iter().map(|term| term.label.as_bytes()))
        .collect();

    for line in scan_lines(resolved) {
        for marker in [b'<', b'>', b'%', b'+', b'|', b'='] {
            if marker_run(resolved, line, marker)
                .map(|run| run.width >= width)
                .unwrap_or(false)
            {
                return Err(DomainError::InvalidResolved {
                    message: format!(
                        "resolved file contains a conflict marker run of `{}` bytes",
                        marker as char
                    ),
                    region_index: None,
                    range: Some((line.full.start, line.full.end)),
                });
            }
        }

        if let Some(label) = continuation_label(resolved, line, &labels) {
            return Err(DomainError::InvalidResolved {
                message: format!(
                    "resolved file contains a JJ continuation label `{}`",
                    escape_bytes(label)
                ),
                region_index: None,
                range: Some((line.full.start, line.full.end)),
            });
        }
    }
    Ok(())
}

fn continuation_label<'a>(input: &'a [u8], line: Line, labels: &[&'a [u8]]) -> Option<&'a [u8]> {
    let mut cursor = line.content.start;
    while cursor < line.content.end && input.get(cursor).copied() == Some(b'\\') {
        cursor = cursor.checked_add(1)?;
    }
    if cursor == line.content.start {
        return None;
    }
    while cursor < line.content.end && matches!(input.get(cursor).copied(), Some(b' ' | b'\t')) {
        cursor = cursor.checked_add(1)?;
    }
    let label_start = cursor.checked_add(3)?;
    if !input.get(cursor..line.content.end)?.starts_with(b"to:") {
        return None;
    }
    let candidate = input.get(label_start..line.content.end)?.trim_ascii();
    labels
        .iter()
        .copied()
        .find(|label| label.trim_ascii() == candidate)
}

/// Render an [`ApplyPlan`] as a deterministic, byte-safe unified diff.
///
/// Valid UTF-8 is kept readable; invalid bytes and control bytes are rendered
/// as explicit escapes. CRLF and missing final newlines are retained in the
/// display and the latter receives the conventional explicit diagnostic line.
pub fn render_unified_diff(
    original: &[u8],
    resolved: &[u8],
    plan: &ApplyPlan,
) -> Result<String, DomainError> {
    plan.validate(original.len())?;
    let mut reconstructed = Vec::new();
    let mut original_cursor = 0;
    let mut resolved_starts = Vec::with_capacity(plan.hunks.len());
    let mut resolved_cursor: usize = 0;
    for hunk in &plan.hunks {
        let original_start = hunk.original_range.start;
        let original_end = hunk.original_range.end;
        let unchanged = original
            .get(original_cursor..original_start)
            .ok_or_else(|| DomainError::invalid("diff hunk range is outside the original bytes"))?;
        let resolved_start = resolved_cursor
            .checked_add(
                original_start
                    .checked_sub(original_cursor)
                    .ok_or_else(|| DomainError::invalid("diff hunks are out of source order"))?,
            )
            .ok_or_else(|| DomainError::invalid("resolved diff offset overflow"))?;
        reconstructed.extend_from_slice(unchanged);
        resolved_starts.push(resolved_start);
        reconstructed.extend_from_slice(&hunk.replacement);
        resolved_cursor = reconstructed.len();
        original_cursor = original_end;
    }
    let unchanged = original
        .get(original_cursor..)
        .ok_or_else(|| DomainError::invalid("diff hunk range is outside the original bytes"))?;
    reconstructed.extend_from_slice(unchanged);
    if reconstructed != resolved {
        return Err(DomainError::InvalidResolved {
            message: "diff plan does not reconstruct the resolved bytes".into(),
            region_index: None,
            range: None,
        });
    }
    if plan.hunks.is_empty() {
        return Ok("No changes.\n".into());
    }

    let mut output = String::from("--- source\n+++ resolved\n");
    for (hunk, &resolved_start) in plan.hunks.iter().zip(&resolved_starts) {
        let old = original
            .get(hunk.original_range.start..hunk.original_range.end)
            .ok_or_else(|| DomainError::invalid("diff hunk range is outside the original bytes"))?;
        let new = &hunk.replacement;
        let old_lines = diff_lines(old);
        let new_lines = diff_lines(new);
        let old_start = line_number_at(original, hunk.original_range.start);
        let new_start = line_number_at(resolved, resolved_start);
        output.push_str(&format!(
            "@@ -{},{} +{},{} @@ bytes [{}..{})\n",
            old_start,
            old_lines.len(),
            new_start,
            new_lines.len(),
            hunk.original_range.start,
            hunk.original_range.end
        ));
        for line in old_lines {
            output.push('-');
            output.push_str(&escape_bytes(line.bytes));
            output.push('\n');
            if !line.has_eol {
                output.push_str("\\ No newline at end of file\n");
            }
        }
        for line in new_lines {
            output.push('+');
            output.push_str(&escape_bytes(line.bytes));
            output.push('\n');
            if !line.has_eol {
                output.push_str("\\ No newline at end of file\n");
            }
        }
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiffLine<'a> {
    bytes: &'a [u8],
    has_eol: bool,
}

fn diff_lines(input: &[u8]) -> Vec<DiffLine<'_>> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(DiffLine {
                bytes: &input[start..index],
                has_eol: true,
            });
            start = index + 1;
        }
    }
    if start < input.len() {
        lines.push(DiffLine {
            bytes: &input[start..],
            has_eol: false,
        });
    }
    lines
}

fn line_number_at(input: &[u8], offset: usize) -> usize {
    1 + input[..offset.min(input.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
}

fn escape_bytes(input: &[u8]) -> String {
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < input.len() {
        let Some(&byte) = input.get(cursor) else {
            break;
        };
        match byte {
            b' '..=b'~' if byte != b'\\' => {
                output.push(byte as char);
                cursor += 1;
            }
            b'\\' => {
                output.push_str("\\\\");
                cursor += 1;
            }
            b'\t' => {
                output.push_str("\\t");
                cursor += 1;
            }
            b'\r' => {
                output.push_str("\\r");
                cursor += 1;
            }
            _ => {
                if let Some(length) = utf8_char_length(input, cursor) {
                    if let Some(bytes) = input.get(cursor..cursor.saturating_add(length)) {
                        if let Ok(text) = std::str::from_utf8(bytes) {
                            if let Some(character) = text.chars().next() {
                                if !character.is_control() {
                                    output.push(character);
                                    cursor = cursor.saturating_add(length);
                                    continue;
                                }
                            }
                        }
                    }
                }
                output.push_str(&format!("\\x{byte:02x}"));
                cursor = cursor.saturating_add(1);
            }
        }
    }
    output
}

fn utf8_char_length(input: &[u8], start: usize) -> Option<usize> {
    let byte = *input.get(start)?;
    let length = match byte {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    start
        .checked_add(length)
        .filter(|end| *end <= input.len())
        .map(|_| length)
}

fn hex_digest(digest: [u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::domain::{ManifestRegion, ManifestTerm};

    fn manifest_for(source: &[u8]) -> Manifest {
        let document = parse_snapshot(source).unwrap();
        let regions = document
            .regions
            .iter()
            .enumerate()
            .map(|(region_index, region)| ManifestRegion {
                region_index,
                source_range: region.source_range,
                seed: region_seed(
                    region_index,
                    &region.terms,
                    region_trailing_eol(&document.source, region.source_range),
                )
                .into_boxed_slice(),
                terms: region
                    .terms
                    .iter()
                    .map(|term| {
                        ManifestTerm::from_term(
                            region_index,
                            term,
                            crate::domain::Sha256Digest(sha256(&term.logical_bytes)),
                        )
                    })
                    .collect(),
            })
            .collect();
        Manifest::new(
            crate::domain::MANIFEST_SCHEMA_VERSION,
            crate::domain::SourceIdentity::new("/repo/file"),
            crate::domain::Sha256Digest(sha256(source)),
            document.marker,
            source.len(),
            regions,
        )
        .unwrap()
    }

    fn fixture() -> (Vec<u8>, Manifest) {
        let source = b"prefix\n<<<<<<< conflict\n+++++++ side\nold\n------- base\nbase\n>>>>>>> close\nsuffix\n";
        (source.to_vec(), manifest_for(source))
    }

    #[test]
    fn accepts_a_single_region_edit_and_reports_one_original_hunk() {
        let (source, manifest) = fixture();
        let resolved = b"prefix\nnew\nsuffix\n";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved,
        })
        .unwrap();
        assert_eq!(plan.hunks.len(), 1);
        assert_eq!(
            plan.hunks[0].original_range,
            manifest.regions[0].source_range
        );
        assert_eq!(&*plan.hunks[0].replacement, b"new\n");
    }

    #[test]
    fn accepts_multiple_regions_with_insertions_deletions_and_length_changes() {
        let source = b"a\n<<<<<<< one\n+++++++ side\nx\n------- base\ny\n>>>>>>> end\nb\n<<<<<<< two\n+++++++ side\np\n------- base\nq\n>>>>>>> end\nz\n";
        let manifest = manifest_for(source);
        let resolved = b"a\nreplacement with more bytes\nb\nremoved\nz\n";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved,
        })
        .unwrap();
        assert_eq!(plan.hunks.len(), 2);
        assert_eq!(
            &*plan.hunks[0].replacement,
            b"replacement with more bytes\n"
        );
        assert_eq!(&*plan.hunks[1].replacement, b"removed\n");
        assert_ne!(plan.hunks[0].new_len, plan.hunks[0].old_len);
    }

    #[test]
    fn rejects_stale_source_before_treating_resolved_as_installable() {
        let (source, manifest) = fixture();
        let mut stale = source.clone();
        stale[0] = b'P';
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &stale,
            resolved: b"prefix\nnew\nsuffix\n",
        })
        .unwrap_err();
        assert!(matches!(error, DomainError::StaleSource { .. }));
    }

    #[test]
    fn rejects_prefix_and_suffix_changes_with_first_byte_context() {
        let (source, manifest) = fixture();
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: b"PREFIX\nnew\nsuffix\n",
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::GuardViolation {
                region_index: 0,
                start: 0,
                ..
            }
        ));

        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: b"prefix\nnew\nSUFFIX\n",
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::GuardViolation {
                region_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn rejects_marker_runs_at_active_width_but_allows_short_marker_like_payload() {
        let (source, manifest) = fixture();
        let short = b"prefix\n<<<< payload\nsuffix\n";
        assert!(
            validate_apply(ApplyValidationRequest {
                manifest: &manifest,
                original_source: &source,
                resolved: short,
            })
            .is_ok()
        );

        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: b"prefix\n<<<<<<< payload\nsuffix\n",
        })
        .unwrap_err();
        assert!(matches!(error, DomainError::InvalidResolved { .. }));
    }

    #[test]
    fn rejects_resolved_prefix_suffix_overlap_without_panicking() {
        let source = b"prefix\n<<<<<<< conflict\n+++++++ side\nold\n------- base\nbase\n>>>>>>> close\n\nsuffix\n";
        let manifest = manifest_for(source);
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved: b"prefix\nsuffix\n",
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::GuardViolation {
                region_index: 0,
                ref message,
                ..
            } if message.contains("boundaries overlap")
        ));
    }

    #[test]
    fn rejects_configured_continuation_labels_and_all_marker_families() {
        let (source, manifest) = fixture();
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: b"prefix\n\\       to: side\nsuffix\n",
        })
        .unwrap_err();
        assert!(matches!(error, DomainError::InvalidResolved { .. }));

        for marker in [b'>', b'%', b'+', b'|', b'='] {
            let mut resolved = b"prefix\n       payload\nsuffix\n".to_vec();
            resolved[7..14].fill(marker);
            let error = validate_apply(ApplyValidationRequest {
                manifest: &manifest,
                original_source: &source,
                resolved: &resolved,
            })
            .unwrap_err();
            assert!(
                matches!(error, DomainError::InvalidResolved { .. }),
                "marker {marker}"
            );
        }
    }

    #[test]
    fn rejects_malformed_manifest_ranges_and_paths_at_apply_boundary() {
        let (source, mut manifest) = fixture();
        manifest.schema_version = 99;
        assert!(matches!(
            validate_apply(ApplyValidationRequest {
                manifest: &manifest,
                original_source: &source,
                resolved: b"prefix\nnew\nsuffix\n",
            }),
            Err(DomainError::InvalidManifest { .. })
        ));

        let (_, mut manifest) = fixture();
        manifest.regions[0].source_range = ByteRange {
            start: 1,
            end: 1000,
        };
        assert!(matches!(
            validate_apply(ApplyValidationRequest {
                manifest: &manifest,
                original_source: &source,
                resolved: b"prefix\nnew\nsuffix\n",
            }),
            Err(DomainError::InvalidManifest { .. })
        ));

        let (_, mut manifest) = fixture();
        manifest.regions[0].terms[0].artifact_path = "../escape.term".into();
        assert!(matches!(
            validate_apply(ApplyValidationRequest {
                manifest: &manifest,
                original_source: &source,
                resolved: b"prefix\nnew\nsuffix\n",
            }),
            Err(DomainError::InvalidManifest { .. })
        ));
    }

    #[test]
    fn renders_deterministic_byte_safe_diff_for_crlf_and_missing_final_newline() {
        let source = b"prefix\r\n<<<<<<< conflict\r\n+++++++ side\r\nold\r\n------- base\r\nbase\r\n>>>>>>> close";
        let manifest = manifest_for(source);
        let resolved = b"prefix\r\nnew\x80";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved,
        })
        .unwrap();
        let rendered = render_unified_diff(source, resolved, &plan).unwrap();
        assert_eq!(
            rendered,
            render_unified_diff(source, resolved, &plan).unwrap()
        );
        assert!(rendered.contains(
            "--- source
+++ resolved
"
        ));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\x80"));
        assert!(rendered.contains("No newline at end of file"));
    }

    #[test]
    fn apply_rejects_untouched_seed() {
        let (source, manifest) = fixture();
        let unresolved = materialize_unresolved(&parse_snapshot(&source).unwrap()).unwrap();
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: &unresolved,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::InvalidResolved {
                region_index: Some(0),
                ..
            }
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("JCW"));
        assert!(rendered.contains("region 0"));
        assert!(rendered.contains("regions/region-000/term-000.term"));
        assert!(rendered.contains("regions/region-000/term-001.term"));
    }

    #[test]
    fn apply_rejects_untouched_adjacent_regions_seed() {
        // Two back-to-back regions (no bytes between their spans) merge into a
        // single conflict group; every untouched region's seed must still be
        // refused, and a fully replaced group must be accepted.
        let source = b"x\n<<<<<<< one\n+++++++ side\na\n------- base\nb\n>>>>>>> end\n<<<<<<< two\n+++++++ side\nc\n------- base\nd\n>>>>>>> end\ny\n";
        let manifest = manifest_for(source);
        let document = parse_snapshot(source).unwrap();
        assert_eq!(conflict_groups(&manifest).len(), 1);

        let unresolved = materialize_unresolved(&document).unwrap();
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved: &unresolved,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::InvalidResolved {
                region_index: Some(0),
                ..
            }
        ));

        // Region 0 replaced but region 1's seed untouched: the merged group
        // still contains region 1's recorded seed.
        let partially = [
            b"x\n".as_slice(),
            b"replaced\n".as_slice(),
            region_seed(1, &document.regions[1].terms, b"\n").as_slice(),
            b"y\n".as_slice(),
        ]
        .concat();
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved: &partially,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::InvalidResolved {
                region_index: Some(1),
                ..
            }
        ));
        assert!(error.to_string().contains("region 1"));

        let replaced = b"x\nreplaced\ny\n";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved: replaced,
        })
        .unwrap();
        assert_eq!(plan.hunks.len(), 1);
        assert_eq!(&*plan.hunks[0].replacement, b"replaced\n");
    }

    #[test]
    fn apply_rejects_partially_resolved() {
        let source = b"a\n<<<<<<< one\n+++++++ side\nx\n------- base\ny\n>>>>>>> end\nb\n<<<<<<< two\n+++++++ side\np\n------- base\nq\n>>>>>>> end\nz\n";
        let manifest = manifest_for(source);
        let document = parse_snapshot(source).unwrap();
        let resolved = [
            b"a\n".as_slice(),
            b"replacement\n".as_slice(),
            b"b\n".as_slice(),
            region_seed(1, &document.regions[1].terms, b"\n").as_slice(),
            b"z\n".as_slice(),
        ]
        .concat();
        let error = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved: &resolved,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DomainError::InvalidResolved {
                region_index: Some(1),
                ..
            }
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("region 1"));
        assert!(rendered.contains("regions/region-001/term-001.term"));
    }

    #[test]
    fn apply_accepts_fully_resolved() {
        // Content replacement, deletion, and keeping one term verbatim are all
        // valid resolutions; the hunks match the three regions.
        let source = b"a\n<<<<<<< one\n+++++++ side\nx\n------- base\ny\n>>>>>>> end\nb\n<<<<<<< two\n+++++++ side\np\n------- base\nq\n>>>>>>> end\nc\n<<<<<<< three\n+++++++ side\nm\n------- base\nn\n>>>>>>> end\nd\n";
        let manifest = manifest_for(source);
        let resolved = b"a\nreplacement\nb\nc\nm\nd\n";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved,
        })
        .unwrap();
        assert_eq!(plan.hunks.len(), 3);
        assert_eq!(&*plan.hunks[0].replacement, b"replacement\n");
        assert!(plan.hunks[1].replacement.is_empty());
        assert_eq!(&*plan.hunks[2].replacement, b"m\n");
    }

    #[test]
    fn apply_accepts_seed_not_matching_any_recorded_seed() {
        let (source, manifest) = fixture();
        // Content that differs from every recorded seed is accepted, even when
        // it contains a seed-like prefix that is not the full seed.
        let resolved = b"prefix\nJCW-UNRESOLVED-CONFLICT-REGION-000: partial text\nsuffix\n";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved,
        })
        .unwrap();
        assert_eq!(plan.hunks.len(), 1);
        assert_eq!(
            &*plan.hunks[0].replacement,
            b"JCW-UNRESOLVED-CONFLICT-REGION-000: partial text\n"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ManifestRegion, ManifestTerm};
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
    fn preserves_regions_and_seed_bytes() {
        let input = b"prefix\n<<<<<<< open\n+++++++ left\nleft\n------- base\nbase\n+++++++ right\nright\n>>>>>>> close\nsuffix\n";
        let document = parse_snapshot(input).unwrap();
        let seed = b"JCW-UNRESOLVED-CONFLICT-REGION-000: replace this line with the final content for this conflict, or delete the line to drop the content. Terms: regions/region-000/term-000.term, regions/region-000/term-001.term, regions/region-000/term-002.term\n";
        assert_eq!(
            materialize_unresolved(&document).unwrap(),
            [b"prefix\n".as_slice(), seed, b"suffix\n"].concat()
        );
        let range = document.regions[0].source_range;
        assert_eq!(&input[..range.start], b"prefix\n");
        assert_eq!(&input[range.end..], b"suffix\n");
    }

    #[test]
    fn rejects_invalid_structure_with_context() {
        let cases = [
            (
                b"<<<<<<<\n+++++++ side\nx\n>>>>>>\n".as_slice(),
                Some(0),
                Some(b"<<<<<<<\n+++++++ side\nx\n>>>>>>\n".len()),
            ),
            (
                b"<<<<<<<\n------- base\nx\n+++++++ side\ny\n>>>>>>>\n".as_slice(),
                Some(0),
                Some(8),
            ),
        ];
        for (input, expected_region, expected_offset) in cases {
            let error = parse_snapshot(input).unwrap_err();
            assert!(
                matches!(
                    error,
                    DomainError::InvalidInput {
                        region_index,
                        byte_offset,
                        ..
                    } if region_index == expected_region && byte_offset == expected_offset
                ),
                "unexpected malformed-input error: {error:?}"
            );
        }
        assert!(matches!(
            parse_snapshot(b"ordinary bytes"),
            Err(DomainError::NoConflictFound { .. })
        ));
    }

    #[test]
    fn treats_sub_minimum_opening_runs_as_ordinary_content() {
        // Real files (Svelte, HTML, JSX, XML) legitimately start lines with
        // '<'. A run shorter than MIN_MARKER_WIDTH can never be a jj
        // snapshot-style marker, so it must be preserved as content around
        // and between regions instead of failing the parse.
        let input = b"<script lang=\"ts\">\n<<<<<<< conflict 1 of 1\n+++++++ side\nx\n------- base\nbase\n+++++++ other\ny\n>>>>>>> conflict 1 of 1 ends\n<div>\n";
        let document = parse_snapshot(input).unwrap();
        assert_eq!(document.regions.len(), 1);
        assert_eq!(
            materialize_unresolved(&document).unwrap(),
            [
                b"<script lang=\"ts\">\n".as_slice(),
                region_seed(
                    0,
                    &document.regions[0].terms,
                    region_trailing_eol(&document.source, document.regions[0].source_range)
                )
                .as_slice(),
                b"<div>\n"
            ]
            .concat()
        );
        assert_eq!(
            &input[..document.regions[0].source_range.start],
            b"<script lang=\"ts\">\n"
        );
        assert_eq!(&input[document.regions[0].source_range.end..], b"<div>\n");

        // A markerless file that merely starts with '<' is a no-conflict
        // outcome, not a malformed-input error.
        assert!(matches!(
            parse_snapshot(b"<script>\nconst x = 1;\n"),
            Err(DomainError::NoConflictFound { .. })
        ));
    }

    #[test]
    fn rejects_mixed_opening_widths_across_regions_with_context() {
        let input = b"<<<<<<< first\n+++++++ side\none\n------- base\nbase\n>>>>>>> close\n<<<<<<<< second\n++++++++ side\ntwo\n-------- base\nbase\n>>>>>>>> close\n";
        let expected_offset = input
            .windows(b"<<<<<<<< second".len())
            .position(|window| window == b"<<<<<<<< second")
            .unwrap();
        let error = parse_snapshot(input).unwrap_err();
        assert!(matches!(
            error,
            DomainError::InvalidInput {
                region_index: Some(1),
                byte_offset: Some(actual),
                ref message,
                ..
            } if actual == expected_offset && message.contains("expected document width 7")
        ));
    }

    #[test]
    fn rejects_over_width_section_and_closing_markers_with_context() {
        let section = b"<<<<<<<\n++++++++ side\nx\n------- base\ny\n>>>>>>>\n";
        assert!(matches!(
            parse_snapshot(section),
            Err(DomainError::InvalidInput {
                region_index: Some(0),
                byte_offset: Some(8),
                ref message,
                ..
            }) if message.contains("section marker run has width 8")
        ));

        let closing = b"<<<<<<<\n+++++++ side\nx\n------- base\ny\n>>>>>>>> close\n";
        let expected_offset = closing
            .windows(b">>>>>>>> close".len())
            .position(|window| window == b">>>>>>>> close")
            .unwrap();
        assert!(matches!(
            parse_snapshot(closing),
            Err(DomainError::InvalidInput {
                region_index: Some(0),
                byte_offset: Some(actual),
                ref message,
                ..
            }) if actual == expected_offset && message.contains("closing marker run has width 8")
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
        assert!(materialize_unresolved(&document).is_err());

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
        assert!(materialize_unresolved(&document).is_err());

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
        assert!(materialize_unresolved(&overlapping).is_err());

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
        assert!(materialize_unresolved(&marker_mismatch).is_err());
    }

    #[test]
    fn region_seed_is_self_describing_marker_free_and_deterministic() {
        let terms = vec![
            Term::new(0, TermKind::Side, "side", b"x".to_vec(), false).unwrap(),
            Term::new(1, TermKind::Base, "base", b"y".to_vec(), false).unwrap(),
            Term::new(2, TermKind::Side, "side 2", b"z".to_vec(), false).unwrap(),
        ];
        let seed = region_seed(2, &terms, b"\n");
        let text = String::from_utf8(seed.clone()).unwrap();
        assert!(text.contains("JCW-UNRESOLVED-CONFLICT-REGION-002"));
        assert!(text.contains("regions/region-002/term-000.term"));
        assert!(text.contains("regions/region-002/term-001.term"));
        assert!(text.contains("regions/region-002/term-002.term"));
        assert!(text.ends_with('\n'));
        assert!(text.is_ascii());
        assert!(
            !text.contains('\\'),
            "seed must not form a continuation shape"
        );
        for marker in [b'<', b'>', b'%', b'+', b'|', b'='] {
            assert!(
                !text.contains(marker as char),
                "seed must contain no marker-family byte `{}`: {text:?}",
                marker as char
            );
            assert!(
                seed.windows(MIN_MARKER_WIDTH)
                    .all(|window| !window.iter().all(|byte| *byte == marker)),
                "seed must contain no marker-family run of width {MIN_MARKER_WIDTH}"
            );
        }
        assert_eq!(region_seed(2, &terms, b"\n"), seed, "seed is deterministic");
    }

    #[test]
    fn region_seed_mirrors_the_region_trailing_eol() {
        let lf = b"prefix\n<<<<<<< open\n+++++++ side\nx\n------- base\ny\n>>>>>>> close\nsuffix\n";
        let document = parse_snapshot(lf).unwrap();
        let region = &document.regions[0];
        assert_eq!(
            region_trailing_eol(&document.source, region.source_range),
            b"\n"
        );
        assert!(region_seed(0, &region.terms, b"\n").ends_with(b"\n"));

        let crlf = b"prefix\r\n<<<<<<< open\r\n+++++++ side\r\nx\r\n------- base\r\ny\r\n>>>>>>> close\r\nsuffix\r\n";
        let document = parse_snapshot(crlf).unwrap();
        let region = &document.regions[0];
        assert_eq!(
            region_trailing_eol(&document.source, region.source_range),
            b"\r\n"
        );
        assert!(region_seed(0, &region.terms, b"\r\n").ends_with(b"\r\n"));

        let no_eol = b"prefix\n<<<<<<< open\n+++++++ side\nx\n------- base\ny\n>>>>>>> close";
        let document = parse_snapshot(no_eol).unwrap();
        let region = &document.regions[0];
        assert_eq!(
            region_trailing_eol(&document.source, region.source_range),
            b""
        );
        assert!(!region_seed(0, &region.terms, b"").ends_with(b"\n"));
    }

    #[test]
    fn materialize_unresolved_mirrors_eol_forms_and_preserves_outside_bytes() {
        let crlf = b"prefix\r\n<<<<<<< open\r\n+++++++ side\r\nx\r\n------- base\r\ny\r\n>>>>>>> close\r\nsuffix\r\n";
        let document = parse_snapshot(crlf).unwrap();
        let seed = region_seed(0, &document.regions[0].terms, b"\r\n");
        assert_eq!(
            materialize_unresolved(&document).unwrap(),
            [b"prefix\r\n".as_slice(), seed.as_slice(), b"suffix\r\n"].concat()
        );

        let no_eol = b"prefix\n<<<<<<< open\n+++++++ side\nx\n------- base\ny\n>>>>>>> close";
        let document = parse_snapshot(no_eol).unwrap();
        let seed = region_seed(0, &document.regions[0].terms, b"");
        assert_eq!(
            materialize_unresolved(&document).unwrap(),
            [b"prefix\n".as_slice(), seed.as_slice()].concat()
        );

        // Out-of-range document spans must not panic the materializer.
        let marker = SnapshotMarker::default();
        let term = Term::new(0, TermKind::Side, "side", b"x".to_vec(), false).unwrap();
        let malformed = ParsedDocument {
            source: b"x".to_vec().into_boxed_slice(),
            regions: vec![ConflictRegion {
                source_range: ByteRange {
                    start: usize::MAX,
                    end: usize::MAX,
                },
                marker,
                terms: vec![term],
            }],
            marker,
        };
        assert!(materialize_unresolved(&malformed).is_err());
    }

    type MetadataJson = serde_json::Value;

    fn metadata_field<'a>(value: &'a MetadataJson, key: &str) -> &'a MetadataJson {
        value
            .get(key)
            .unwrap_or_else(|| panic!("metadata field {key:?} is missing"))
    }

    fn metadata_string(value: &MetadataJson, key: &str) -> String {
        value
            .get(key)
            .and_then(MetadataJson::as_str)
            .unwrap_or_else(|| panic!("metadata field {key:?} is not a string: {value:?}"))
            .to_owned()
    }

    fn metadata_number(value: &MetadataJson, key: &str) -> usize {
        value
            .get(key)
            .and_then(MetadataJson::as_u64)
            .and_then(|number| usize::try_from(number).ok())
            .unwrap_or_else(|| panic!("metadata field {key:?} is not a usize: {value:?}"))
    }

    fn metadata_bool(value: &MetadataJson, key: &str) -> bool {
        value
            .get(key)
            .and_then(MetadataJson::as_bool)
            .unwrap_or_else(|| panic!("metadata field {key:?} is not a boolean: {value:?}"))
    }

    fn assert_artifact_metadata(artifact: &MetadataJson, bytes: &[u8], context: &str) {
        assert_eq!(
            bytes.len(),
            metadata_number(artifact, "byte_length"),
            "{context}: byte length"
        );
        let digest = sha256(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            digest,
            metadata_string(artifact, "sha256"),
            "{context}: SHA-256"
        );
        let has_crlf = bytes.windows(2).any(|window| window == b"\r\n");
        let has_lone_cr = bytes
            .iter()
            .enumerate()
            .any(|(index, &byte)| byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'));
        let has_lone_lf = bytes.iter().enumerate().any(|(index, &byte)| {
            byte == b'\n' && bytes.get(index.checked_sub(1).unwrap_or(usize::MAX)) != Some(&b'\r')
        });
        let line_ending_mode = match (has_crlf, has_lone_lf, has_lone_cr) {
            (false, false, false) => "none",
            (true, false, false) => "crlf",
            (false, true, false) => "lf",
            _ => "mixed",
        };
        assert_eq!(
            line_ending_mode,
            metadata_string(artifact, "line_ending_mode"),
            "{context}: line-ending mode"
        );
        assert_eq!(
            bytes.last() == Some(&b'\n'),
            metadata_bool(artifact, "final_newline"),
            "{context}: final newline"
        );
    }

    fn metadata_array<'a>(value: &'a MetadataJson, key: &str) -> &'a [MetadataJson] {
        value
            .get(key)
            .and_then(MetadataJson::as_array)
            .map(Vec::as_slice)
            .unwrap_or_else(|| panic!("metadata field {key:?} is not an array: {value:?}"))
    }

    fn metadata_artifact<'a>(metadata: &'a MetadataJson, path: &str) -> &'a MetadataJson {
        metadata_field(metadata, "artifacts")
            .as_object()
            .and_then(|artifacts| {
                artifacts.values().find(|artifact| {
                    artifact.get("path").and_then(MetadataJson::as_str) == Some(path)
                })
            })
            .unwrap_or_else(|| panic!("metadata artifact {path:?} is missing"))
    }

    fn metadata_file(path: &Path) -> MetadataJson {
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
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
        let input_artifact =
            metadata_field(metadata_field(&metadata, "artifacts"), "input.snapshot");
        assert_artifact_metadata(input_artifact, &input, &format!("{name}: input"));
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
                let artifact_metadata =
                    metadata_artifact(&metadata, &metadata_string(expected_term, "path"));
                assert_artifact_metadata(
                    artifact_metadata,
                    &artifact,
                    &format!("{name}: region {region_index} term {ordinal}"),
                );
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
        assert_artifact_metadata(resolved_artifact, &resolved, &format!("{name}: resolved"));
        assert_eq!(
            materialize_unresolved(&document).unwrap(),
            resolved,
            "{name}: resolved seed"
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
        let mut supported_count = 0;
        for case in &cases {
            match metadata_string(case, "status").as_str() {
                "supported" => {
                    supported_count += 1;
                    assert_supported_metadata_case(case);
                }
                "reference" => {}
                other => panic!(
                    "{}: unknown corpus status {other:?}",
                    metadata_string(case, "case_name")
                ),
            }
        }
        assert_eq!(
            supported_count, 8,
            "index must enumerate all supported corpus cases"
        );
    }

    #[test]
    fn metadata_driven_reference_corpus_is_rejected() {
        let cases = corpus_cases();
        let mut reference_count = 0;
        for index_case in &cases {
            if metadata_string(index_case, "status") != "reference" {
                continue;
            }
            reference_count += 1;
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
        assert_eq!(
            reference_count, 8,
            "index must enumerate all reference corpus cases"
        );
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
                materialize_unresolved(&document).unwrap(),
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
                materialize_unresolved(&document).unwrap(),
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
            let terms_for_seed: Vec<Term> = region_terms
                .iter()
                .enumerate()
                .map(|(ordinal, (kind, label, logical))| {
                    Term::new(ordinal, *kind, label.clone(), logical.clone(), synthetic).unwrap()
                })
                .collect();
            let trailing_eol: &[u8] = if synthetic { b"" } else { eol };
            expected_resolved.extend_from_slice(&region_seed(
                region_index,
                &terms_for_seed,
                trailing_eol,
            ));
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
            materialize_unresolved(&document).unwrap(),
            generated.expected_resolved,
            "generated case {case_index}: seed"
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

    fn manifest_for_core_tests(source: &[u8]) -> Manifest {
        let document = parse_snapshot(source).expect("test snapshot should parse");
        let regions = document
            .regions
            .iter()
            .enumerate()
            .map(|(region_index, region)| ManifestRegion {
                region_index,
                source_range: region.source_range,
                seed: region_seed(
                    region_index,
                    &region.terms,
                    region_trailing_eol(&document.source, region.source_range),
                )
                .into_boxed_slice(),
                terms: region
                    .terms
                    .iter()
                    .map(|term| {
                        ManifestTerm::from_term(
                            region_index,
                            term,
                            crate::domain::Sha256Digest(sha256(&term.logical_bytes)),
                        )
                    })
                    .collect(),
            })
            .collect();
        Manifest::new(
            crate::domain::MANIFEST_SCHEMA_VERSION,
            crate::domain::SourceIdentity::new("source"),
            crate::domain::Sha256Digest(sha256(source)),
            document.marker,
            source.len(),
            regions,
        )
        .expect("test manifest should validate")
    }

    #[test]
    fn apply_accepts_empty_replacement_and_deletion() {
        let source =
            b"before\n<<<<<<< one\n+++++++ side\nremove me\n------- base\nbase\n>>>>>>> end\nafter\n";
        let manifest = manifest_for_core_tests(source);
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved: b"before\nafter\n",
        })
        .expect("deletion should be valid");
        assert_eq!(plan.hunks.len(), 1);
        assert!(plan.hunks[0].replacement.is_empty());
        assert_eq!(
            plan.hunks[0].old_len,
            manifest.regions[0].source_range.len()
        );
    }

    #[test]
    fn apply_maps_multiple_regions_repeated_bases_and_invalid_bytes() {
        let source = b"p\n<<<<<<< one\n+++++++ side\n\xff\n------- base\nbase\n------- base2\n\x80\n>>>>>>> end\nm\n<<<<<<< two\n+++++++ side\nold\r\n------- base\nbase\r\n>>>>>>> end\nq\n";
        let manifest = manifest_for_core_tests(source);
        let resolved = b"p\n\xff\nm\nreplacement\nq\n";
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: source,
            resolved,
        })
        .expect("multi-region replacement should be valid");
        assert_eq!(plan.hunks.len(), 2);
        let rendered = render_unified_diff(source, resolved, &plan).expect("diff should render");
        assert!(rendered.contains("\\xff"));
        assert!(rendered.contains("replacement"));
    }

    #[test]
    fn public_core_boundaries_return_errors_instead_of_panicking() {
        let marker = SnapshotMarker::default();
        let term = Term::new(0, TermKind::Side, "side", b"x".to_vec(), false).unwrap();
        let malformed_document = ParsedDocument {
            source: b"x".to_vec().into_boxed_slice(),
            regions: vec![ConflictRegion {
                source_range: ByteRange {
                    start: usize::MAX,
                    end: usize::MAX,
                },
                marker,
                terms: vec![term.clone()],
            }],
            marker,
        };
        let malformed_plan = ApplyPlan {
            hunks: vec![DiffHunk {
                sequence: 0,
                original_range: ByteRange {
                    start: usize::MAX,
                    end: usize::MAX,
                },
                replacement: Box::new([]),
                old_len: 0,
                new_len: 0,
            }],
        };
        let manifest = Manifest {
            schema_version: crate::domain::MANIFEST_SCHEMA_VERSION,
            source: crate::domain::SourceIdentity::new("source"),
            source_digest: crate::domain::Sha256Digest(sha256(b"x")),
            marker,
            source_length: 1,
            regions: vec![ManifestRegion {
                region_index: 0,
                source_range: ByteRange {
                    start: usize::MAX,
                    end: usize::MAX,
                },
                seed: b"seed".to_vec().into_boxed_slice(),
                terms: vec![ManifestTerm::from_term(
                    0,
                    &term,
                    crate::domain::Sha256Digest::ZERO,
                )],
            }],
        };

        let materialize = std::panic::catch_unwind(|| materialize_unresolved(&malformed_document));
        assert!(materialize.is_ok());
        assert!(materialize.unwrap().is_err());

        let validate = std::panic::catch_unwind(|| {
            validate_apply(ApplyValidationRequest {
                manifest: &manifest,
                original_source: b"x",
                resolved: b"y",
            })
        });
        assert!(validate.is_ok());
        assert!(validate.unwrap().is_err());

        let render = std::panic::catch_unwind(|| render_unified_diff(b"x", b"", &malformed_plan));
        assert!(render.is_ok());
        assert!(render.unwrap().is_err());

        for input in [b"".as_slice(), b"<", b"<<<<<<<\n+++++++\xff\n"] {
            let parsed = std::panic::catch_unwind(|| parse_snapshot(input));
            assert!(parsed.is_ok());
            assert!(parsed.unwrap().is_err());
        }
    }
}
