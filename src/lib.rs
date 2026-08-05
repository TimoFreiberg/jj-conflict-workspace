//! Dependency-free foundations for parsing and safely untangling JJ conflicts.
//!
//! The public API is intentionally byte-oriented. Filesystem access, `jj`
//! process execution, serialization, and merge policy are deferred to later
//! layers; this crate only defines their stable contracts.

pub mod apply;
pub mod cli;
pub mod core;
pub mod domain;
pub mod error;
pub(crate) mod path_output;
mod prepare;
pub(crate) mod repository_root;

pub use apply::ApplyReport;
pub use cli::{ApplyOptions, Command, PrepareOptions, USAGE, parse_args};
pub use core::{materialize_scaffold, parse_snapshot, render_unified_diff, validate_apply};
pub use domain::{
    ApplyPlan, ApplyValidationRequest, ByteRange, ConflictRegion, DiffHunk,
    MANIFEST_SCHEMA_VERSION, MIN_MARKER_WIDTH, Manifest, ManifestRegion, ManifestTerm,
    ParsedDocument, Sha256Digest, SnapshotMarker, SnapshotStyle, SourceIdentity, Term, TermKind,
};
pub use error::{CliError, DomainError};
pub use path_output::encode_path_for_output;
pub use prepare::run as prepare;

#[cfg(test)]
mod domain_tests;

#[cfg(test)]
mod error_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn domain_error_display_includes_category_and_context() {
        let errors = vec![
            (
                DomainError::InvalidInput {
                    message: "bad bytes".into(),
                    region_index: Some(2),
                    byte_offset: Some(9),
                    range: None,
                },
                "invalid input: bad bytes (region 2) at byte offset 9",
            ),
            (
                DomainError::UnsupportedStyle {
                    style: "merge".into(),
                },
                "unsupported snapshot style `merge`; only Snapshot is supported",
            ),
            (
                DomainError::PathUnavailable {
                    path: PathBuf::from("file"),
                    message: "missing".into(),
                },
                "path unavailable `file`: missing",
            ),
            (
                DomainError::ExternalCommand {
                    command: "jj".into(),
                    status: Some(1),
                    stderr: "failed".into(),
                },
                "external command `jj` failed with status 1: failed",
            ),
            (
                DomainError::InvalidManifest {
                    message: "bad schema".into(),
                    region_index: Some(1),
                    path: Some(PathBuf::from("manifest")),
                },
                "invalid manifest: bad schema (region 1) (`manifest`)",
            ),
            (
                DomainError::InvalidResolved {
                    message: "bad change".into(),
                    region_index: Some(1),
                    range: Some((2, 4)),
                },
                "invalid resolved file: bad change (region 1) at byte range [2, 4)",
            ),
            (
                DomainError::StaleSource {
                    message: "changed".into(),
                    expected: "a".into(),
                    actual: "b".into(),
                },
                "stale source: changed (expected a, found b)",
            ),
            (
                DomainError::GuardViolation {
                    region_index: 1,
                    start: 4,
                    end: 5,
                    message: "outside".into(),
                },
                "outside-region guard violation in region 1 at [4, 5): outside",
            ),
            (
                DomainError::OutsideRegionChanged {
                    region_index: 1,
                    start: 4,
                    end: 5,
                },
                "outside-region change detected in region 1 at [4, 5)",
            ),
            (
                DomainError::UnsafeWrite {
                    path: PathBuf::from("target"),
                    message: "refused".into(),
                },
                "unsafe write refused for `target`: refused",
            ),
            (
                DomainError::NotImplemented {
                    operation: "parse_snapshot",
                },
                "parse_snapshot is not implemented yet",
            ),
            (
                DomainError::InstallationAmbiguous {
                    path: PathBuf::from("target"),
                    phase: "rename",
                    workspace: PathBuf::from("workspace"),
                    may_have_committed: true,
                    message: "inspect manually".into(),
                },
                "installation ambiguous for `target` during rename (workspace `workspace`): inspect manually; replacement may already be present: true; inspect the source and workspace rather than assuming the file is unchanged",
            ),
        ];
        for (error, expected) in errors {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn cli_error_display_is_actionable() {
        assert_eq!(
            CliError::MissingCommand.to_string(),
            "missing command; choose `prepare` or `apply`"
        );
        assert_eq!(CliError::HelpRequested.to_string(), "help requested");
        assert_eq!(
            CliError::MissingValue {
                option: "--file".into()
            }
            .to_string(),
            "option `--file` requires a value"
        );
        assert_eq!(
            CliError::UnknownOption {
                option: "--bad".into()
            }
            .to_string(),
            "unknown option `--bad`"
        );
        assert_eq!(
            CliError::DuplicateOption {
                option: "--file".into()
            }
            .to_string(),
            "option `--file` was provided more than once"
        );
        assert_eq!(
            CliError::IncompatibleOption {
                option: "--write".into(),
                command: "prepare".into()
            }
            .to_string(),
            "option `--write` is not valid for `prepare`"
        );
    }
}

#[cfg(test)]
mod public_api_tests {
    use super::*;

    #[test]
    fn public_surface_exposes_foundation_contracts() {
        let range = ByteRange::new(0, 1).unwrap();
        let term = Term::new(0, TermKind::Side, "label", Vec::<u8>::new(), false).unwrap();
        let marker = SnapshotMarker::new(SnapshotStyle::Snapshot, 7, 7).unwrap();
        let region = ConflictRegion::new(range, marker, vec![term]).unwrap();
        let document = ParsedDocument::new(Vec::<u8>::new(), vec![], marker);
        assert!(document.is_ok());
        assert_eq!(region.range().len(), 1);
        assert_eq!(MANIFEST_SCHEMA_VERSION, 1);
        assert_eq!(MIN_MARKER_WIDTH, 7);
    }

    #[test]
    fn pure_boundaries_expose_implemented_core_and_deferred_apply() {
        assert!(matches!(
            parse_snapshot(b"anything"),
            Err(DomainError::InvalidInput { .. })
        ));

        let source =
            b"prefix\n<<<<<<< open\n+++++++ side\nx\n------- base\ny\n>>>>>>> close\nsuffix\n";
        let document = parse_snapshot(source).unwrap();
        assert_eq!(
            materialize_scaffold(&document).unwrap(),
            b"prefix\nx\nsuffix\n"
        );

        let source = Vec::<u8>::new();
        let manifest = Manifest::empty(SourceIdentity::new("source"), source.len());
        let request = ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: &source,
        };
        assert!(matches!(
            validate_apply(request),
            Err(DomainError::StaleSource { .. })
        ));
    }
}
