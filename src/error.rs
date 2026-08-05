use std::fmt;
use std::path::PathBuf;

use crate::path_output::encode_path_for_output;

/// Errors raised by the pure domain and validation boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// The supplied representation violates a local invariant.
    InvalidInput {
        message: String,
        region_index: Option<usize>,
        byte_offset: Option<usize>,
        range: Option<(usize, usize)>,
    },
    /// A snapshot style other than the supported snapshot grammar was used.
    UnsupportedStyle { style: String },
    /// An imperative shell could not obtain or validate a path.
    PathUnavailable { path: PathBuf, message: String },
    /// An external command failed at the imperative boundary.
    ExternalCommand {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    /// A workspace manifest is malformed or inconsistent.
    InvalidManifest {
        message: String,
        region_index: Option<usize>,
        path: Option<PathBuf>,
    },
    /// A resolved file cannot satisfy the apply contract.
    InvalidResolved {
        message: String,
        region_index: Option<usize>,
        range: Option<(usize, usize)>,
    },
    /// The original source no longer matches the source recorded by a manifest.
    StaleSource {
        message: String,
        expected: String,
        actual: String,
    },
    /// The source changed while the external JJ snapshot command was running.
    SourceChanged {
        path: PathBuf,
        phase: &'static str,
        expected: String,
        observed: String,
    },
    /// A resolved change escaped the conflict region that authorized it.
    GuardViolation {
        region_index: usize,
        start: usize,
        end: usize,
        message: String,
    },
    /// Alias-level category for callers that distinguish outside-region changes.
    OutsideRegionChanged {
        region_index: usize,
        start: usize,
        end: usize,
    },
    /// A write was refused because it could not meet the safety contract.
    UnsafeWrite { path: PathBuf, message: String },
    /// The replacement may have committed, so the caller must inspect the source and workspace.
    InstallationAmbiguous {
        path: PathBuf,
        phase: &'static str,
        workspace: PathBuf,
        may_have_committed: bool,
        message: String,
    },
    /// A foundation boundary exists but its later algorithm is not implemented.
    NotImplemented { operation: &'static str },
}

impl DomainError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
            region_index: None,
            byte_offset: None,
            range: None,
        }
    }

    pub(crate) fn invalid_region(message: impl Into<String>, region_index: usize) -> Self {
        Self::InvalidInput {
            message: message.into(),
            region_index: Some(region_index),
            byte_offset: None,
            range: None,
        }
    }

    pub(crate) fn invalid_range(message: impl Into<String>, start: usize, end: usize) -> Self {
        Self::InvalidInput {
            message: message.into(),
            region_index: None,
            byte_offset: None,
            range: Some((start, end)),
        }
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput {
                message,
                region_index,
                byte_offset,
                range,
            } => {
                write!(f, "invalid input: {message}")?;
                if let Some(index) = region_index {
                    write!(f, " (region {index})")?;
                }
                if let Some(offset) = byte_offset {
                    write!(f, " at byte offset {offset}")?;
                }
                if let Some((start, end)) = range {
                    write!(f, " at byte range [{start}, {end})")?;
                }
                Ok(())
            }
            Self::UnsupportedStyle { style } => write!(
                f,
                "unsupported snapshot style `{style}`; only Snapshot is supported"
            ),
            Self::PathUnavailable { path, message } => {
                write!(
                    f,
                    "path unavailable `{}`: {message}",
                    encode_path_for_output(path)
                )
            }
            Self::ExternalCommand {
                command,
                status,
                stderr,
            } => {
                write!(f, "external command `{command}` failed")?;
                if let Some(status) = status {
                    write!(f, " with status {status}")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::InvalidManifest {
                message,
                region_index,
                path,
            } => {
                write!(f, "invalid manifest: {message}")?;
                if let Some(index) = region_index {
                    write!(f, " (region {index})")?;
                }
                if let Some(path) = path {
                    write!(f, " (`{}`)", encode_path_for_output(path))?;
                }
                Ok(())
            }
            Self::InvalidResolved {
                message,
                region_index,
                range,
            } => {
                write!(f, "invalid resolved file: {message}")?;
                if let Some(index) = region_index {
                    write!(f, " (region {index})")?;
                }
                if let Some((start, end)) = range {
                    write!(f, " at byte range [{start}, {end})")?;
                }
                Ok(())
            }
            Self::StaleSource {
                message,
                expected,
                actual,
            } => write!(
                f,
                "stale source: {message} (expected {expected}, found {actual})"
            ),
            Self::SourceChanged {
                path,
                phase,
                expected,
                observed,
            } => write!(
                f,
                "source changed during JJ ({phase}) for `{}`: expected identity {expected}, observed identity {observed}",
                encode_path_for_output(path)
            ),
            Self::GuardViolation {
                region_index,
                start,
                end,
                message,
            } => write!(
                f,
                "outside-region guard violation in region {region_index} at [{start}, {end}): {message}"
            ),
            Self::OutsideRegionChanged {
                region_index,
                start,
                end,
            } => write!(
                f,
                "outside-region change detected in region {region_index} at [{start}, {end})"
            ),
            Self::UnsafeWrite { path, message } => write!(
                f,
                "unsafe write refused for `{}`: {message}",
                encode_path_for_output(path)
            ),
            Self::InstallationAmbiguous {
                path,
                phase,
                workspace,
                may_have_committed,
                message,
            } => {
                write!(
                    f,
                    "installation ambiguous for `{}` during {phase} (workspace `{}`): {message}; replacement may already be present: {may_have_committed}; inspect the source and workspace rather than assuming the file is unchanged",
                    encode_path_for_output(path),
                    encode_path_for_output(workspace),
                )
            }
            Self::NotImplemented { operation } => write!(f, "{operation} is not implemented yet"),
        }
    }
}

impl std::error::Error for DomainError {}

/// Syntax and configuration errors are kept separate from domain errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingCommand,
    HelpRequested,
    MissingValue { option: String },
    UnknownOption { option: String },
    DuplicateOption { option: String },
    IncompatibleOption { option: String, command: String },
    UnexpectedArgument { argument: String },
    EmptyArgument,
    UnknownCommand { command: String },
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => write!(f, "missing command; choose `prepare` or `apply`"),
            Self::HelpRequested => write!(f, "help requested"),
            Self::MissingValue { option } => write!(f, "option `{option}` requires a value"),
            Self::UnknownOption { option } => write!(f, "unknown option `{option}`"),
            Self::DuplicateOption { option } => {
                write!(f, "option `{option}` was provided more than once")
            }
            Self::IncompatibleOption { option, command } => {
                write!(f, "option `{option}` is not valid for `{command}`")
            }
            Self::UnexpectedArgument { argument } => write!(f, "unexpected argument `{argument}`"),
            Self::EmptyArgument => write!(f, "an argument must not be empty"),
            Self::UnknownCommand { command } => write!(
                f,
                "unknown command `{command}`; choose `prepare` or `apply`"
            ),
        }
    }
}

impl std::error::Error for CliError {}
