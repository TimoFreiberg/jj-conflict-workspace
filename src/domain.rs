use std::fmt;
use std::path::{Component, PathBuf};

use crate::error::DomainError;

/// The minimum marker run width supported by the snapshot grammar.
pub const MIN_MARKER_WIDTH: usize = 7;
/// Version of the internal manifest contract.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// A checked half-open byte range, `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl ByteRange {
    pub fn new(start: usize, end: usize) -> Result<Self, DomainError> {
        if start > end {
            return Err(DomainError::invalid_range(
                "range start exceeds range end",
                start,
                end,
            ));
        }
        Ok(Self { start, end })
    }

    pub fn from_start_len(start: usize, len: usize) -> Result<Self, DomainError> {
        let end = start
            .checked_add(len)
            .ok_or_else(|| DomainError::invalid("byte range end overflow"))?;
        Self::new(start, end)
    }

    /// Return the length of a valid range. Malformed public struct literals
    /// yield zero here; callers crossing a validation boundary receive a
    /// typed error from `validate` instead of a subtraction panic.
    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }
    pub fn validate(self) -> Result<(), DomainError> {
        if self.start > self.end {
            return Err(DomainError::invalid_range(
                "range start exceeds range end",
                self.start,
                self.end,
            ));
        }
        Ok(())
    }
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
    pub fn contains_offset(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
    pub fn contains_range(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
    pub fn within_source(self, source_len: usize) -> bool {
        self.end <= source_len
    }
    pub fn checked_end(self) -> Result<usize, DomainError> {
        self.start
            .checked_add(self.len())
            .ok_or_else(|| DomainError::invalid("byte range end overflow"))
    }
    pub fn slice<'a>(self, source: &'a [u8]) -> Result<&'a [u8], DomainError> {
        self.validate()?;
        if !self.within_source(source.len()) {
            return Err(DomainError::invalid("range exceeds source length"));
        }
        Ok(&source[self.start..self.end])
    }

    pub(crate) fn require_non_empty(self, what: &str) -> Result<(), DomainError> {
        self.validate()?;
        if self.is_empty() {
            Err(DomainError::invalid(format!(
                "{what} range must be non-empty"
            )))
        } else {
            Ok(())
        }
    }
}

/// Semantic category encoded by a snapshot section, without ours/theirs roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TermKind {
    Side,
    Base,
}

impl fmt::Display for TermKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Side => "side",
            Self::Base => "base",
        })
    }
}

/// Marker grammar supported by this foundation. Only `Snapshot` is supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotStyle {
    Snapshot,
}

impl fmt::Display for SnapshotStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Snapshot")
    }
}

/// Shared marker metadata. Parser grammar rules remain a Task 02 concern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotMarker {
    pub style: SnapshotStyle,
    pub outer_marker_width: usize,
    pub section_marker_width: usize,
}

impl Default for SnapshotMarker {
    fn default() -> Self {
        Self {
            style: SnapshotStyle::Snapshot,
            outer_marker_width: MIN_MARKER_WIDTH,
            section_marker_width: MIN_MARKER_WIDTH,
        }
    }
}

impl SnapshotMarker {
    pub fn new(
        style: SnapshotStyle,
        outer_marker_width: usize,
        section_marker_width: usize,
    ) -> Result<Self, DomainError> {
        let marker = Self {
            style,
            outer_marker_width,
            section_marker_width,
        };
        marker.validate()?;
        Ok(marker)
    }

    pub fn validate(self) -> Result<(), DomainError> {
        if self.outer_marker_width < MIN_MARKER_WIDTH
            || self.section_marker_width < MIN_MARKER_WIDTH
        {
            return Err(DomainError::invalid(format!(
                "marker widths must be at least {MIN_MARKER_WIDTH}"
            )));
        }
        if self.outer_marker_width != self.section_marker_width {
            return Err(DomainError::invalid(
                "outer and section marker widths must be equal",
            ));
        }
        Ok(())
    }
}

/// An ordered logical section. `logical_bytes` are already after any one
/// synthetic separator EOL has been removed; no bytes are normalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub ordinal: usize,
    pub kind: TermKind,
    pub label: String,
    pub logical_bytes: Box<[u8]>,
    pub synthetic_separator_eol_removed: bool,
}

impl Term {
    pub fn new(
        ordinal: usize,
        kind: TermKind,
        label: impl Into<String>,
        logical_bytes: impl Into<Box<[u8]>>,
        synthetic_separator_eol_removed: bool,
    ) -> Result<Self, DomainError> {
        Ok(Self {
            ordinal,
            kind,
            label: label.into(),
            logical_bytes: logical_bytes.into(),
            synthetic_separator_eol_removed,
        })
    }

    /// Build a term from exact UTF-8 label bytes without lossy conversion.
    pub fn from_label_bytes(
        ordinal: usize,
        kind: TermKind,
        label: &[u8],
        logical_bytes: impl Into<Box<[u8]>>,
        synthetic_separator_eol_removed: bool,
        region_index: Option<usize>,
        byte_offset: usize,
    ) -> Result<Self, DomainError> {
        let label = String::from_utf8(label.to_vec()).map_err(|_| DomainError::InvalidInput {
            message: "section label is not valid UTF-8".into(),
            region_index,
            byte_offset: Some(byte_offset),
            range: None,
        })?;
        Self::new(
            ordinal,
            kind,
            label,
            logical_bytes,
            synthetic_separator_eol_removed,
        )
    }

    pub fn logical_final_newline(&self) -> bool {
        self.logical_bytes.last() == Some(&b'\n')
    }
    pub fn logical_len(&self) -> usize {
        self.logical_bytes.len()
    }

    pub fn validate_ordinals(terms: &[Self]) -> Result<(), DomainError> {
        for (expected, term) in terms.iter().enumerate() {
            if term.ordinal != expected {
                return Err(DomainError::invalid(format!(
                    "term ordinal {} is not contiguous; expected {expected}",
                    term.ordinal
                )));
            }
        }
        Ok(())
    }

    pub fn assemble(terms: Vec<Self>) -> Result<Vec<Self>, DomainError> {
        Self::validate_ordinals(&terms)?;
        Ok(terms)
    }
}

/// A complete parsed conflict region, including its source marker span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRegion {
    pub source_range: ByteRange,
    pub marker: SnapshotMarker,
    pub terms: Vec<Term>,
}

impl ConflictRegion {
    pub fn new(
        source_range: ByteRange,
        marker: SnapshotMarker,
        terms: Vec<Term>,
    ) -> Result<Self, DomainError> {
        let region = Self {
            source_range,
            marker,
            terms,
        };
        region.validate()?;
        Ok(region)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        self.source_range.validate()?;
        self.source_range.require_non_empty("conflict region")?;
        self.marker.validate()?;
        if self.terms.is_empty() {
            return Err(DomainError::invalid(
                "conflict region must contain at least one term",
            ));
        }
        Term::validate_ordinals(&self.terms)
    }

    pub fn range(&self) -> ByteRange {
        self.source_range
    }
}

/// Owned source bytes plus validated, source-ordered conflict regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedDocument {
    pub source: Box<[u8]>,
    pub regions: Vec<ConflictRegion>,
    pub marker: SnapshotMarker,
}

impl ParsedDocument {
    pub fn new(
        source: impl Into<Box<[u8]>>,
        regions: Vec<ConflictRegion>,
        marker: SnapshotMarker,
    ) -> Result<Self, DomainError> {
        let document = Self {
            source: source.into(),
            regions,
            marker,
        };
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        self.marker.validate()?;
        let mut previous_end = 0;
        for (index, region) in self.regions.iter().enumerate() {
            region.validate()?;
            if region.marker != self.marker {
                return Err(DomainError::invalid_region(
                    "region marker does not match document marker",
                    index,
                ));
            }
            if !region.source_range.within_source(self.source.len()) {
                return Err(DomainError::invalid_region(
                    "region range exceeds source length",
                    index,
                ));
            }
            if index > 0 && region.source_range.start < previous_end {
                return Err(DomainError::invalid_region(
                    "regions overlap or are out of source order",
                    index,
                ));
            }
            previous_end = region.source_range.end;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    pub canonical_path: PathBuf,
    pub repository_relative: Option<PathBuf>,
}

impl SourceIdentity {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            canonical_path: path.into(),
            repository_relative: None,
        }
    }
    pub fn with_repository_relative(mut self, path: impl Into<PathBuf>) -> Self {
        self.repository_relative = Some(path.into());
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sha256Digest(pub [u8; 32]);

impl Sha256Digest {
    pub const ZERO: Self = Self([0; 32]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestTerm {
    pub ordinal: usize,
    pub kind: TermKind,
    pub label: String,
    pub logical_length: usize,
    pub digest: Sha256Digest,
    pub logical_final_newline: bool,
    pub synthetic_separator_eol_removed: bool,
    pub artifact_path: PathBuf,
}

impl ManifestTerm {
    /// Create manifest metadata from a validated logical term.
    ///
    /// Digest computation is intentionally supplied by the caller; this
    /// foundation does not select a hashing implementation.
    pub fn from_term(region_index: usize, term: &Term, digest: Sha256Digest) -> Self {
        Self {
            ordinal: term.ordinal,
            kind: term.kind,
            label: term.label.clone(),
            logical_length: term.logical_len(),
            digest,
            logical_final_newline: term.logical_final_newline(),
            synthetic_separator_eol_removed: term.synthetic_separator_eol_removed,
            artifact_path: Self::generated_artifact_path(region_index, term.ordinal),
        }
    }

    pub fn generated_artifact_path(region_index: usize, ordinal: usize) -> PathBuf {
        PathBuf::from(format!(
            "regions/region-{region_index:03}/term-{ordinal:03}.term"
        ))
    }

    fn validate_path(&self, region_index: usize) -> Result<(), DomainError> {
        if self.artifact_path.is_absolute()
            || self.artifact_path.has_root()
            || self.artifact_path.components().any(|component| {
                matches!(
                    component,
                    Component::Prefix(_) | Component::RootDir | Component::ParentDir
                )
            })
        {
            return Err(DomainError::InvalidManifest {
                message:
                    "artifact path must be relative and cannot contain ParentDir or a root/prefix"
                        .into(),
                region_index: Some(region_index),
                path: Some(self.artifact_path.clone()),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestRegion {
    pub region_index: usize,
    pub source_range: ByteRange,
    pub terms: Vec<ManifestTerm>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u32,
    pub source: SourceIdentity,
    pub source_digest: Sha256Digest,
    pub marker: SnapshotMarker,
    pub source_length: usize,
    pub regions: Vec<ManifestRegion>,
}

impl Manifest {
    pub fn empty(source: SourceIdentity, source_length: usize) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            source,
            source_digest: Sha256Digest::ZERO,
            marker: SnapshotMarker::default(),
            source_length,
            regions: Vec::new(),
        }
    }

    pub fn new(
        schema_version: u32,
        source: SourceIdentity,
        source_digest: Sha256Digest,
        marker: SnapshotMarker,
        source_length: usize,
        regions: Vec<ManifestRegion>,
    ) -> Result<Self, DomainError> {
        let manifest = Self {
            schema_version,
            source,
            source_digest,
            marker,
            source_length,
            regions,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(DomainError::InvalidManifest {
                message: format!(
                    "unsupported schema version {}; expected {MANIFEST_SCHEMA_VERSION}",
                    self.schema_version
                ),
                region_index: None,
                path: None,
            });
        }
        self.marker
            .validate()
            .map_err(|error| DomainError::InvalidManifest {
                message: error.to_string(),
                region_index: None,
                path: None,
            })?;
        let mut previous_end = 0;
        for (expected_index, region) in self.regions.iter().enumerate() {
            if region.region_index != expected_index {
                return Err(DomainError::InvalidManifest {
                    message: format!(
                        "region index {} is not contiguous; expected {expected_index}",
                        region.region_index
                    ),
                    region_index: Some(region.region_index),
                    path: None,
                });
            }
            region
                .source_range
                .validate()
                .map_err(|error| DomainError::InvalidManifest {
                    message: error.to_string(),
                    region_index: Some(region.region_index),
                    path: None,
                })?;
            region
                .source_range
                .require_non_empty("manifest region")
                .map_err(|error| DomainError::InvalidManifest {
                    message: error.to_string(),
                    region_index: Some(region.region_index),
                    path: None,
                })?;
            if !region.source_range.within_source(self.source_length) {
                return Err(DomainError::InvalidManifest {
                    message: "region range exceeds manifest source length".into(),
                    region_index: Some(region.region_index),
                    path: None,
                });
            }
            if expected_index > 0 && region.source_range.start < previous_end {
                return Err(DomainError::InvalidManifest {
                    message: "regions overlap or are out of source order".into(),
                    region_index: Some(region.region_index),
                    path: None,
                });
            }
            previous_end = region.source_range.end;
            if region.terms.is_empty() {
                return Err(DomainError::InvalidManifest {
                    message: "manifest region must contain at least one term".into(),
                    region_index: Some(region.region_index),
                    path: None,
                });
            }
            for (expected_ordinal, term) in region.terms.iter().enumerate() {
                if term.ordinal != expected_ordinal {
                    return Err(DomainError::InvalidManifest {
                        message: format!(
                            "term ordinal {} is not contiguous; expected {expected_ordinal}",
                            term.ordinal
                        ),
                        region_index: Some(region.region_index),
                        path: Some(term.artifact_path.clone()),
                    });
                }
                term.validate_path(region.region_index)?;
                let expected_path =
                    ManifestTerm::generated_artifact_path(region.region_index, term.ordinal);
                if term.artifact_path != expected_path {
                    return Err(DomainError::InvalidManifest {
                        message: format!("artifact path must be `{}`", expected_path.display()),
                        region_index: Some(region.region_index),
                        path: Some(term.artifact_path.clone()),
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyValidationRequest<'a> {
    pub manifest: &'a Manifest,
    pub original_source: &'a [u8],
    pub resolved: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub sequence: usize,
    pub original_range: ByteRange,
    pub replacement: Box<[u8]>,
    pub old_len: usize,
    pub new_len: usize,
}

impl DiffHunk {
    pub fn new(
        sequence: usize,
        original_range: ByteRange,
        replacement: impl Into<Box<[u8]>>,
    ) -> Self {
        let replacement = replacement.into();
        Self {
            sequence,
            old_len: original_range.len(),
            new_len: replacement.len(),
            original_range,
            replacement,
        }
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        self.original_range.validate()?;
        if self.old_len != self.original_range.len() {
            return Err(DomainError::invalid(
                "diff hunk old_len does not match original range length",
            ));
        }
        if self.new_len != self.replacement.len() {
            return Err(DomainError::invalid(
                "diff hunk new_len does not match replacement length",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyPlan {
    pub hunks: Vec<DiffHunk>,
}

impl ApplyPlan {
    pub fn new(hunks: Vec<DiffHunk>) -> Result<Self, DomainError> {
        let plan = Self { hunks };
        plan.validate_intrinsic()?;
        Ok(plan)
    }

    pub fn empty() -> Self {
        Self { hunks: Vec::new() }
    }

    pub fn validate_intrinsic(&self) -> Result<(), DomainError> {
        let mut previous_end = 0;
        for (expected_sequence, hunk) in self.hunks.iter().enumerate() {
            hunk.validate()?;
            if hunk.sequence != expected_sequence {
                return Err(DomainError::invalid(format!(
                    "hunk sequence {} is not contiguous; expected {expected_sequence}",
                    hunk.sequence
                )));
            }
            if hunk.original_range.start < previous_end {
                return Err(DomainError::invalid(
                    "diff hunks overlap or are out of source order",
                ));
            }
            previous_end = hunk.original_range.end;
        }
        Ok(())
    }

    pub fn validate(&self, source_length: usize) -> Result<(), DomainError> {
        self.validate_intrinsic()?;
        for hunk in &self.hunks {
            if !hunk.original_range.within_source(source_length) {
                return Err(DomainError::invalid("diff hunk exceeds source length"));
            }
        }
        Ok(())
    }
}
