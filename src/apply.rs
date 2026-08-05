use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use filetime::{FileTime, set_file_handle_times};
use serde::Deserialize;
use tempfile::Builder;

use crate::cli::ApplyOptions;
use crate::core::{render_unified_diff, validate_apply};
use crate::domain::{
    ApplyPlan, ApplyValidationRequest, ByteRange, MANIFEST_SCHEMA_VERSION, Manifest,
    ManifestRegion, ManifestTerm, Sha256Digest, SnapshotMarker, SnapshotStyle, SourceIdentity,
    TermKind,
};
use crate::error::DomainError;
use crate::prepare::sha256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    pub source_path: PathBuf,
    pub manifest_path: PathBuf,
    pub changed_hunks: usize,
    pub old_bytes: usize,
    pub new_bytes: usize,
    pub wrote: bool,
    pub dry_run: bool,
    pub diff: String,
}

#[derive(Debug)]
struct ApplyContext {
    resolved_file: PathBuf,
    manifest_file: PathBuf,
}

#[derive(Debug)]
struct ValidatedInput {
    source_file: PathBuf,
    source: Vec<u8>,
    resolved: Vec<u8>,
    manifest_digest: [u8; 32],
    source_digest: [u8; 32],
    resolved_digest: [u8; 32],
    plan: ApplyPlan,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestJson {
    schema_version: u32,
    source: SourceJson,
    source_sha256: String,
    source_length: usize,
    region_count: usize,
    marker: MarkerJson,
    regions: Vec<RegionJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceJson {
    canonical_path: String,
    canonical_path_bytes_hex: String,
    repository_relative: Option<String>,
    repository_relative_bytes_hex: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerJson {
    style: String,
    outer_marker_width: usize,
    section_marker_width: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegionJson {
    region_index: usize,
    source_range: RangeJson,
    term_count: usize,
    terms: Vec<TermJson>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeJson {
    start: usize,
    end: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TermJson {
    ordinal: usize,
    kind: String,
    label: String,
    logical_length: usize,
    sha256: String,
    logical_final_newline: bool,
    synthetic_separator_eol_removed: bool,
    artifact_path: PathBuf,
}

pub fn run(options: &ApplyOptions) -> Result<ApplyReport, DomainError> {
    let context = discover(options)?;
    let first = context.read_and_validate()?;
    let diff = render_diff(&first, &context)?;
    if !options.write {
        return Ok(ApplyReport {
            source_path: first.source_file,
            manifest_path: context.manifest_file,
            changed_hunks: first.plan.hunks.len(),
            old_bytes: first.source.len(),
            new_bytes: first.resolved.len(),
            wrote: false,
            dry_run: true,
            diff,
        });
    }

    // The first pass is intentionally observational. A write pass reads every
    // artifact again and reruns the pure validator immediately before mutation.
    let second = context.read_and_validate()?;
    if second.manifest_digest != first.manifest_digest
        || second.source_digest != first.source_digest
        || second.resolved_digest != first.resolved_digest
    {
        return Err(DomainError::UnsafeWrite {
            path: second.source_file,
            message: "manifest, source, or resolved input changed between validation passes".into(),
        });
    }
    let second_diff = render_diff(&second, &context)?;
    if second.plan.hunks.is_empty() {
        return Ok(ApplyReport {
            source_path: second.source_file,
            manifest_path: context.manifest_file,
            changed_hunks: 0,
            old_bytes: second.source.len(),
            new_bytes: second.resolved.len(),
            wrote: false,
            dry_run: false,
            diff: second_diff,
        });
    }
    atomic_install(&second.source_file, &second.resolved)?;
    Ok(ApplyReport {
        source_path: second.source_file,
        manifest_path: context.manifest_file,
        changed_hunks: second.plan.hunks.len(),
        old_bytes: second.source.len(),
        new_bytes: second.resolved.len(),
        wrote: true,
        dry_run: false,
        diff: second_diff,
    })
}

fn discover(options: &ApplyOptions) -> Result<ApplyContext, DomainError> {
    let resolved_file = absolute_path(&options.resolved_file)?;
    require_regular_file(&resolved_file, "resolved file")?;
    let manifest_file = match &options.manifest {
        Some(path) => absolute_path(path)?,
        None => resolved_file
            .parent()
            .ok_or_else(|| path_failure(&resolved_file, "resolved file has no parent directory"))?
            .join("manifest.json"),
    };
    require_regular_file(&manifest_file, "manifest")?;

    let resolved_parent = canonical_parent(&resolved_file)?;
    let manifest_parent = canonical_parent(&manifest_file)?;
    if resolved_parent != manifest_parent {
        return Err(path_failure(
            &manifest_file,
            format!(
                "manifest and resolved file must be in the same workspace (manifest parent `{}`, resolved parent `{}`)",
                manifest_parent.display(),
                resolved_parent.display()
            ),
        ));
    }

    Ok(ApplyContext {
        resolved_file,
        manifest_file,
    })
}

impl ApplyContext {
    fn read_and_validate(&self) -> Result<ValidatedInput, DomainError> {
        let manifest_bytes = read_regular_file(&self.manifest_file, "manifest")?;
        let manifest_digest = sha256(&manifest_bytes);
        let manifest = decode_manifest(&manifest_bytes, &self.manifest_file)?;
        let source_file = source_path(&manifest, &self.manifest_file)?;
        validate_artifacts(&manifest, &self.manifest_file)?;
        let source = read_regular_file(&source_file, "source")?;
        if fs::canonicalize(&self.resolved_file)
            .map_err(|error| path_error(&self.resolved_file, error))?
            == source_file
        {
            return Err(path_failure(
                &self.resolved_file,
                "resolved file must not be the manifest source file",
            ));
        }
        let resolved = read_regular_file(&self.resolved_file, "resolved file")?;
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source,
            resolved: &resolved,
        })
        .map_err(|error| {
            contextualize(
                error,
                &self.manifest_file,
                &source_file,
                &self.resolved_file,
            )
        })?;
        let source_digest = sha256(&source);
        let resolved_digest = sha256(&resolved);
        Ok(ValidatedInput {
            source_file,
            source,
            source_digest,
            resolved_digest,
            resolved,
            manifest_digest,
            plan,
        })
    }
}

fn render_diff(input: &ValidatedInput, context: &ApplyContext) -> Result<String, DomainError> {
    render_unified_diff(&input.source, &input.resolved, &input.plan).map_err(|error| {
        contextualize(
            error,
            &context.manifest_file,
            &input.source_file,
            &context.resolved_file,
        )
    })
}

fn decode_manifest(bytes: &[u8], path: &Path) -> Result<Manifest, DomainError> {
    let json: ManifestJson = serde_json::from_slice(bytes)
        .map_err(|error| path_failure(path, format!("could not parse manifest JSON: {error}")))?;
    if json.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(path_failure(
            path,
            format!(
                "unsupported manifest schema version {}; expected {MANIFEST_SCHEMA_VERSION}",
                json.schema_version
            ),
        ));
    }
    if json.source.canonical_path.is_empty() {
        return Err(path_failure(
            path,
            "manifest source canonical_path is empty",
        ));
    }
    let canonical_bytes = decode_hex(
        &json.source.canonical_path_bytes_hex,
        "canonical source path",
    )
    .map_err(|message| path_failure(path, message))?;
    let canonical_path = os_string_from_bytes(canonical_bytes)
        .map(PathBuf::from)
        .map_err(|message| {
            path_failure(
                path,
                format!("invalid canonical source path bytes: {message}"),
            )
        })?;
    if !canonical_path.is_absolute() {
        return Err(path_failure(
            path,
            "manifest canonical source path must be absolute",
        ));
    }
    if let Some(text) = canonical_path.to_str()
        && json.source.canonical_path != text
    {
        return Err(path_failure(
            path,
            "manifest canonical_path does not match canonical_path_bytes_hex",
        ));
    }

    let repository_relative = match (
        json.source.repository_relative,
        json.source.repository_relative_bytes_hex,
    ) {
        (None, None) => None,
        (Some(text), Some(hex)) => {
            let bytes = decode_hex(&hex, "repository-relative source path")
                .map_err(|message| path_failure(path, message))?;
            let value = os_string_from_bytes(bytes)
                .map(PathBuf::from)
                .map_err(|message| {
                    path_failure(
                        path,
                        format!("invalid repository-relative source path bytes: {message}"),
                    )
                })?;
            if value.is_absolute()
                || value.components().any(|component| {
                    matches!(
                        component,
                        Component::Prefix(_) | Component::RootDir | Component::ParentDir
                    )
                })
            {
                return Err(path_failure(
                    path,
                    "manifest repository-relative source path is not safe",
                ));
            }
            if text.is_empty() {
                return Err(path_failure(
                    path,
                    "manifest repository-relative source path text is empty",
                ));
            }
            if let Some(value_text) = value.to_str()
                && text != value_text
            {
                return Err(path_failure(
                    path,
                    "manifest repository-relative path text does not match its byte field",
                ));
            }
            Some(value)
        }
        _ => {
            return Err(path_failure(
                path,
                "manifest repository-relative path text and byte fields must both be present or absent",
            ));
        }
    };

    let source_digest = parse_digest(&json.source_sha256)
        .map_err(|message| path_failure(path, format!("invalid source_sha256: {message}")))?;
    let style = match json.marker.style.as_str() {
        "Snapshot" => SnapshotStyle::Snapshot,
        other => {
            return Err(path_failure(
                path,
                format!("unsupported marker style `{other}`"),
            ));
        }
    };
    let marker = SnapshotMarker::new(
        style,
        json.marker.outer_marker_width,
        json.marker.section_marker_width,
    )
    .map_err(|error| path_failure(path, format!("invalid marker metadata: {error}")))?;
    if json.region_count != json.regions.len() {
        return Err(path_failure(
            path,
            format!(
                "manifest region_count {} does not match {} regions",
                json.region_count,
                json.regions.len()
            ),
        ));
    }

    let mut regions = Vec::with_capacity(json.regions.len());
    for region in json.regions {
        if region.term_count != region.terms.len() {
            return Err(path_failure(
                path,
                format!(
                    "region {} term_count {} does not match {} terms",
                    region.region_index,
                    region.term_count,
                    region.terms.len()
                ),
            ));
        }
        let mut terms = Vec::with_capacity(region.terms.len());
        for term in region.terms {
            let kind = match term.kind.as_str() {
                "side" => TermKind::Side,
                "base" => TermKind::Base,
                other => {
                    return Err(DomainError::InvalidManifest {
                        message: format!("unsupported term kind `{other}`"),
                        region_index: Some(region.region_index),
                        path: Some(term.artifact_path),
                    });
                }
            };
            let digest =
                parse_digest(&term.sha256).map_err(|message| DomainError::InvalidManifest {
                    message: format!("invalid term sha256: {message}"),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                })?;
            terms.push(ManifestTerm {
                ordinal: term.ordinal,
                kind,
                label: term.label,
                logical_length: term.logical_length,
                digest,
                logical_final_newline: term.logical_final_newline,
                synthetic_separator_eol_removed: term.synthetic_separator_eol_removed,
                artifact_path: term.artifact_path,
            });
        }
        regions.push(ManifestRegion {
            region_index: region.region_index,
            source_range: ByteRange {
                start: region.source_range.start,
                end: region.source_range.end,
            },
            terms,
        });
    }

    let manifest = Manifest {
        schema_version: json.schema_version,
        source: SourceIdentity {
            canonical_path,
            repository_relative,
        },
        source_digest,
        marker,
        source_length: json.source_length,
        regions,
    };
    manifest
        .validate()
        .map_err(|error| contextual_manifest_error(error, path))?;
    Ok(manifest)
}

fn source_path(manifest: &Manifest, manifest_file: &Path) -> Result<PathBuf, DomainError> {
    let path = &manifest.source.canonical_path;
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(path_failure(
            path,
            "manifest source must be a regular file and must not be a symlink",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| path_error(path, error))?;
    if canonical != *path {
        return Err(path_failure(
            path,
            "manifest source path is not the canonical path recorded by prepare",
        ));
    }
    let workspace_parent = canonical_parent(manifest_file)?;
    for component in manifest
        .source
        .repository_relative
        .iter()
        .flat_map(|path| path.components())
    {
        if matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        ) {
            return Err(path_failure(
                manifest_file,
                "manifest repository-relative source path escapes its repository",
            ));
        }
    }
    let _ = workspace_parent;
    Ok(path.clone())
}

fn validate_artifacts(manifest: &Manifest, manifest_file: &Path) -> Result<(), DomainError> {
    let workspace = canonical_parent(manifest_file)?;
    for region in &manifest.regions {
        for term in &region.terms {
            let artifact = workspace.join(&term.artifact_path);
            reject_symlink_components(&workspace, &artifact, manifest_file)?;
            let bytes = read_regular_file(&artifact, "workspace artifact")?;
            if bytes.len() != term.logical_length {
                return Err(DomainError::InvalidManifest {
                    message: format!(
                        "artifact length is {}, manifest records {}",
                        bytes.len(),
                        term.logical_length
                    ),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                });
            }
            if Sha256Digest(sha256(&bytes)) != term.digest {
                return Err(DomainError::InvalidManifest {
                    message: "artifact SHA-256 does not match manifest".into(),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                });
            }
            if (bytes.last() == Some(&b'\n')) != term.logical_final_newline {
                return Err(DomainError::InvalidManifest {
                    message: "artifact final-newline metadata does not match bytes".into(),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                });
            }
        }
    }
    Ok(())
}

fn atomic_install(path: &Path, replacement: &[u8]) -> Result<(), DomainError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(DomainError::UnsafeWrite {
            path: path.to_owned(),
            message: "source changed into a non-regular file or symlink before installation".into(),
        });
    }
    let parent = path.parent().ok_or_else(|| DomainError::UnsafeWrite {
        path: path.to_owned(),
        message: "source has no parent directory for a sibling temporary file".into(),
    })?;
    let mut temporary = Builder::new()
        .prefix(".jcw-apply-")
        .tempfile_in(parent)
        .map_err(|error| write_failure(path, "create temporary sibling", error))?;

    temporary
        .write_all(replacement)
        .map_err(|error| write_failure(path, "write replacement bytes", error))?;
    temporary
        .as_file()
        .set_permissions(metadata.permissions().clone())
        .map_err(|error| write_failure(path, "preserve source permissions", error))?;
    let access_time = FileTime::from_last_access_time(&metadata);
    let modification_time = FileTime::from_last_modification_time(&metadata);
    set_file_handle_times(
        temporary.as_file(),
        Some(access_time),
        Some(modification_time),
    )
    .map_err(|error| write_failure(path, "preserve source timestamps", error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| write_failure(path, "sync replacement bytes", error))?;
    let temporary_path = temporary.into_temp_path();
    replace_atomically(&temporary_path, path)
        .map_err(|error| write_failure(path, "atomically replace source", error))?;
    Ok(())
}

fn replace_atomically(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(temporary, destination)?;
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "destination has no parent"))?;
        File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe extern "system" {
            fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
        }
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (temporary, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic replacement is unsupported on this platform",
        ))
    }
}

fn reject_symlink_components(
    root: &Path,
    path: &Path,
    manifest_file: &Path,
) -> Result<(), DomainError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        path_failure(
            manifest_file,
            format!("artifact path `{}` escapes the workspace", path.display()),
        )
    })?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| path_error(&current, error))?;
        if metadata.file_type().is_symlink() {
            return Err(path_failure(
                &current,
                "workspace artifact path must not traverse symlinks",
            ));
        }
    }
    Ok(())
}

fn require_regular_file(path: &Path, label: &str) -> Result<(), DomainError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(path_failure(path, format!("{label} must not be a symlink")));
    }
    if !metadata.file_type().is_file() {
        return Err(path_failure(
            path,
            format!("{label} must be a regular file"),
        ));
    }
    Ok(())
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>, DomainError> {
    require_regular_file(path, label)?;
    fs::read(path).map_err(|error| path_error(path, error))
}

fn absolute_path(path: &Path) -> Result<PathBuf, DomainError> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|error| path_failure(path, format!("could not resolve path: {error}")))
}

fn canonical_parent(path: &Path) -> Result<PathBuf, DomainError> {
    let parent = path
        .parent()
        .ok_or_else(|| path_failure(path, "path has no parent directory"))?;
    let metadata = fs::symlink_metadata(parent).map_err(|error| path_error(parent, error))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(path_failure(
            parent,
            "workspace parent must be a real directory",
        ));
    }
    fs::canonicalize(parent).map_err(|error| path_error(parent, error))
}

fn decode_hex(value: &str, label: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err(format!("{label} hex has odd length"));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high =
            hex_digit(pair[0]).ok_or_else(|| format!("{label} hex contains a non-hex digit"))?;
        let low =
            hex_digit(pair[1]).ok_or_else(|| format!("{label} hex contains a non-hex digit"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn parse_digest(value: &str) -> Result<Sha256Digest, String> {
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("digest must use lowercase hexadecimal".into());
    }
    let bytes = decode_hex(value, "digest")?;
    if bytes.len() != 32 {
        return Err(format!("expected 32 bytes, found {}", bytes.len()));
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&bytes);
    Ok(Sha256Digest(digest))
}

fn os_string_from_bytes(bytes: Vec<u8>) -> Result<OsString, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(OsString::from)
            .map_err(|_| "path bytes are not valid platform text".into())
    }
}

fn contextual_manifest_error(error: DomainError, path: &Path) -> DomainError {
    match error {
        DomainError::InvalidManifest {
            message,
            region_index,
            path: detail_path,
        } => DomainError::InvalidManifest {
            message,
            region_index,
            path: detail_path.or_else(|| Some(path.to_owned())),
        },
        other => path_failure(path, other.to_string()),
    }
}

fn contextualize(
    error: DomainError,
    manifest: &Path,
    source: &Path,
    resolved: &Path,
) -> DomainError {
    let suffix = format!(
        " [manifest `{}`, source `{}`, resolved `{}`]",
        manifest.display(),
        source.display(),
        resolved.display()
    );
    match error {
        DomainError::InvalidManifest {
            mut message,
            region_index,
            path,
        } => {
            message.push_str(&suffix);
            DomainError::InvalidManifest {
                message,
                region_index,
                path: path.or_else(|| Some(manifest.to_owned())),
            }
        }
        DomainError::InvalidResolved {
            mut message,
            region_index,
            range,
        } => {
            message.push_str(&suffix);
            DomainError::InvalidResolved {
                message,
                region_index,
                range,
            }
        }
        DomainError::StaleSource {
            mut message,
            expected,
            actual,
        } => {
            message.push_str(&suffix);
            DomainError::StaleSource {
                message,
                expected,
                actual,
            }
        }
        DomainError::GuardViolation {
            region_index,
            start,
            end,
            mut message,
        } => {
            message.push_str(&suffix);
            DomainError::GuardViolation {
                region_index,
                start,
                end,
                message,
            }
        }
        other => path_failure(manifest, format!("{other}{suffix}")),
    }
}

fn path_failure(path: &Path, message: impl Into<String>) -> DomainError {
    DomainError::PathUnavailable {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn path_error(path: &Path, error: io::Error) -> DomainError {
    path_failure(path, error.to_string())
}

fn write_failure(path: &Path, phase: &str, error: io::Error) -> DomainError {
    DomainError::UnsafeWrite {
        path: path.to_owned(),
        message: format!("{phase} failed: {error}; the source was not intentionally truncated"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decoder_is_exact_and_rejects_bad_input() {
        assert_eq!(decode_hex("00aF", "value").unwrap(), vec![0, 175]);
        assert!(decode_hex("0", "value").is_err());
        assert!(decode_hex("gg", "value").is_err());
    }
}
