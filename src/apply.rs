use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
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
use crate::path_output::encode_path_for_output;
use crate::prepare::sha256;
use crate::repository_root::resolve_repository_source;

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

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    file_type: &'static str,
    len: u64,
    mode: u32,
    modified_seconds: i128,
    modified_nanos: u32,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn capture(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Self::from_metadata(&metadata)
    }

    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        let modified = metadata.modified()?;
        let duration = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("file modification time predates UNIX epoch: {error}"),
                )
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            return Ok(Self {
                file_type: if metadata.file_type().is_file() {
                    "regular"
                } else {
                    "other"
                },
                len: metadata.len(),
                mode: metadata.permissions().mode(),
                modified_seconds: duration.as_secs() as i128,
                modified_nanos: duration.subsec_nanos(),
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                file_type: if metadata.file_type().is_file() {
                    "regular"
                } else {
                    "other"
                },
                len: metadata.len(),
                mode: 0,
                modified_seconds: duration.as_secs() as i128,
                modified_nanos: duration.subsec_nanos(),
            })
        }
    }
}

impl std::fmt::Display for FileIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "type={},len={},mode={:o},mtime={}.{:09}",
            self.file_type, self.len, self.mode, self.modified_seconds, self.modified_nanos
        )?;
        #[cfg(unix)]
        write!(formatter, ",dev={},ino={}", self.device, self.inode)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    file_type: &'static str,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u32,
    #[cfg(windows)]
    file_index: u64,
}

impl DirectoryIdentity {
    fn capture(path: &Path) -> io::Result<Self> {
        // `File::open` requests file semantics on Windows and cannot open a
        // directory.  Requesting backup semantics gives us a real directory
        // handle; OPEN_REPARSE_POINT prevents the final component from being
        // followed while the handle identity is captured.
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)?
        };
        #[cfg(not(windows))]
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parent is not a directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            return Ok(Self {
                file_type: "directory",
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            return Ok(Self {
                file_type: "directory",
                volume_serial: metadata.volume_serial_number(),
                file_index: metadata.file_index(),
            });
        }
        #[cfg(not(any(unix, windows)))]
        Ok(Self {
            file_type: "directory",
        })
    }
}

#[derive(Debug)]
struct SecureFile {
    bytes: Vec<u8>,
    identity: FileIdentity,
    metadata: Metadata,
    parent: DirectoryIdentity,
}

#[derive(Debug)]
struct InputSnapshot {
    path: PathBuf,
    /// The exact bytes read and validated during the corresponding pass.
    /// Metadata alone is insufficient: an in-place edit can preserve length,
    /// mtime, and permissions on filesystems with coarse timestamps.
    bytes: Vec<u8>,
    identity: FileIdentity,
    parent: DirectoryIdentity,
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
    plan: ApplyPlan,
    snapshots: Vec<InputSnapshot>,
    workspace: PathBuf,
    destination_parent: DirectoryIdentity,
    source_metadata: Metadata,
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

    // A write has a second complete, handle-based validation pass immediately
    // before any temporary file is created.
    let second = context.read_and_validate()?;
    compare_snapshots(&first, &second)?;
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
    install_validated(&second)?;
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
    let manifest_file = match &options.manifest {
        Some(path) => absolute_path(path)?,
        None => resolved_file
            .parent()
            .ok_or_else(|| path_failure(&resolved_file, "resolved file has no parent directory"))?
            .join("manifest.json"),
    };
    let resolved_parent = canonical_parent(&resolved_file)?;
    let manifest_parent = canonical_parent(&manifest_file)?;
    if resolved_parent != manifest_parent {
        return Err(path_failure(
            &manifest_file,
            format!(
                "manifest and resolved file must be in the same workspace (manifest parent `{}`, resolved parent `{}`)",
                encode_path_for_output(&manifest_parent),
                encode_path_for_output(&resolved_parent)
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
        let manifest_secure = read_secure_file(&self.manifest_file, "manifest")?;
        let manifest = decode_manifest(&manifest_secure.bytes, &self.manifest_file)?;
        let resolved_parent = canonical_parent(&self.resolved_file)?;
        let manifest_parent = canonical_parent(&self.manifest_file)?;
        let repository = resolve_repository_source(&manifest.source.canonical_path)
            .map_err(|error| path_error(&manifest.source.canonical_path, error))?;
        if repository.canonical_source != manifest.source.canonical_path {
            return Err(path_failure(
                &manifest.source.canonical_path,
                "manifest source path is not canonical",
            ));
        }
        let recorded_relative = manifest
            .source
            .repository_relative
            .as_ref()
            .ok_or_else(|| {
                path_failure(
                    &self.manifest_file,
                    "manifest is missing repository-relative source identity",
                )
            })?;
        if *recorded_relative != repository.repository_relative {
            return Err(path_failure(
                &self.manifest_file,
                format!(
                    "manifest repository-relative path `{}` does not match canonical repository path `{}`",
                    encode_path_for_output(recorded_relative),
                    encode_path_for_output(&repository.repository_relative)
                ),
            ));
        }
        if manifest_parent != resolved_parent {
            return Err(path_failure(
                &self.manifest_file,
                "manifest and resolved parents changed during validation",
            ));
        }
        let source_secure = read_secure_file(&repository.canonical_source, "source")?;
        if source_secure.bytes.len() != manifest.source_length
            || Sha256Digest(sha256(&source_secure.bytes)) != manifest.source_digest
        {
            return Err(DomainError::StaleSource {
                message: "source bytes do not match the manifest".into(),
                expected: format!("{} bytes and manifest digest", manifest.source_length),
                actual: format!("{} bytes and current digest", source_secure.bytes.len()),
            });
        }
        let resolved_secure = read_secure_file(&self.resolved_file, "resolved file")?;
        if same_canonical(&self.resolved_file, &repository.canonical_source)? {
            return Err(path_failure(
                &self.resolved_file,
                "resolved file must not be the manifest source file",
            ));
        }
        let mut snapshots = vec![
            InputSnapshot {
                path: self.manifest_file.clone(),
                bytes: manifest_secure.bytes.clone(),
                identity: manifest_secure.identity,
                parent: manifest_secure.parent,
            },
            InputSnapshot {
                path: self.resolved_file.clone(),
                bytes: resolved_secure.bytes.clone(),
                identity: resolved_secure.identity,
                parent: resolved_secure.parent,
            },
            InputSnapshot {
                path: repository.canonical_source.clone(),
                bytes: source_secure.bytes.clone(),
                identity: source_secure.identity,
                parent: source_secure.parent,
            },
        ];
        validate_artifacts_secure(&manifest, &self.manifest_file, &mut snapshots)?;
        let plan = validate_apply(ApplyValidationRequest {
            manifest: &manifest,
            original_source: &source_secure.bytes,
            resolved: &resolved_secure.bytes,
        })
        .map_err(|error| {
            contextualize(
                error,
                &self.manifest_file,
                &repository.canonical_source,
                &self.resolved_file,
            )
        })?;
        let source_file = repository.canonical_source;
        let source_metadata = source_secure.metadata.clone();
        let destination_parent = DirectoryIdentity::capture(
            source_file
                .parent()
                .ok_or_else(|| path_failure(&source_file, "source has no parent"))?,
        )
        .map_err(|error| path_error(&source_file, error))?;
        Ok(ValidatedInput {
            source_file,
            source: source_secure.bytes,
            resolved: resolved_secure.bytes,
            plan,
            snapshots,
            workspace: manifest_parent,
            destination_parent,
            source_metadata,
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

fn validate_artifacts_secure(
    manifest: &Manifest,
    manifest_file: &Path,
    snapshots: &mut Vec<InputSnapshot>,
) -> Result<(), DomainError> {
    let workspace = canonical_parent(manifest_file)?;
    for region in &manifest.regions {
        for term in &region.terms {
            let artifact = workspace.join(&term.artifact_path);
            let secure = read_secure_file(&artifact, "workspace artifact")?;
            if secure.bytes.len() != term.logical_length {
                return Err(DomainError::InvalidManifest {
                    message: format!(
                        "artifact length is {}, manifest records {}",
                        secure.bytes.len(),
                        term.logical_length
                    ),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                });
            }
            if Sha256Digest(sha256(&secure.bytes)) != term.digest {
                return Err(DomainError::InvalidManifest {
                    message: "artifact SHA-256 does not match manifest".into(),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                });
            }
            if (secure.bytes.last() == Some(&b'\n')) != term.logical_final_newline {
                return Err(DomainError::InvalidManifest {
                    message: "artifact final-newline metadata does not match bytes".into(),
                    region_index: Some(region.region_index),
                    path: Some(term.artifact_path.clone()),
                });
            }
            snapshots.push(InputSnapshot {
                path: artifact,
                bytes: secure.bytes,
                identity: secure.identity,
                parent: secure.parent,
            });
        }
    }
    Ok(())
}

fn compare_snapshots(first: &ValidatedInput, second: &ValidatedInput) -> Result<(), DomainError> {
    if first.snapshots.len() != second.snapshots.len() {
        return Err(unsafe_write(
            &second.source_file,
            "recheck input",
            "the validated input set changed between validation passes",
        ));
    }
    for (old, new) in first.snapshots.iter().zip(&second.snapshots) {
        if old.path != new.path
            || old.identity != new.identity
            || old.parent != new.parent
            || old.bytes != new.bytes
        {
            return Err(unsafe_write(
                &new.path,
                "recheck input",
                "an input identity or validated bytes changed between validation passes",
            ));
        }
    }
    if first.destination_parent != second.destination_parent {
        return Err(unsafe_write(
            &second.source_file,
            "recheck parent",
            "the destination parent identity changed between validation passes",
        ));
    }
    Ok(())
}

/// The private filesystem boundary for atomic installation.  Keeping every
/// mutating filesystem operation here makes the production sequence executable
/// by the failure-injection backend without weakening the production checks.
trait InstallBackend {
    fn create_temp(&mut self, parent: &Path) -> io::Result<tempfile::NamedTempFile>;
    fn write_complete(
        &mut self,
        temporary: &mut tempfile::NamedTempFile,
        bytes: &[u8],
    ) -> io::Result<()>;
    fn copy_metadata(
        &mut self,
        temporary: &tempfile::NamedTempFile,
        metadata: &Metadata,
    ) -> io::Result<()>;
    fn sync_file(&mut self, temporary: &tempfile::NamedTempFile) -> io::Result<()>;
    fn sync_directory(&mut self, parent: &Path) -> io::Result<()>;
    fn rename(&mut self, temporary: &Path, destination: &Path) -> io::Result<()>;
    fn cleanup(&mut self, temporary: Option<tempfile::TempPath>) -> CleanupStatus;
}

struct ProductionInstallBackend;

impl InstallBackend for ProductionInstallBackend {
    fn create_temp(&mut self, parent: &Path) -> io::Result<tempfile::NamedTempFile> {
        Builder::new().prefix(".jcw-apply-").tempfile_in(parent)
    }

    fn write_complete(
        &mut self,
        temporary: &mut tempfile::NamedTempFile,
        bytes: &[u8],
    ) -> io::Result<()> {
        temporary.as_file_mut().write_all(bytes)?;
        temporary.as_file_mut().flush()
    }

    fn copy_metadata(
        &mut self,
        temporary: &tempfile::NamedTempFile,
        metadata: &Metadata,
    ) -> io::Result<()> {
        temporary
            .as_file()
            .set_permissions(metadata.permissions().clone())?;
        let access_time = FileTime::from_last_access_time(metadata);
        let modification_time = FileTime::from_last_modification_time(metadata);
        set_file_handle_times(
            temporary.as_file(),
            Some(access_time),
            Some(modification_time),
        )
    }

    fn sync_file(&mut self, temporary: &tempfile::NamedTempFile) -> io::Result<()> {
        temporary.as_file().sync_all()
    }

    fn sync_directory(&mut self, parent: &Path) -> io::Result<()> {
        sync_directory(parent)
    }

    fn rename(&mut self, temporary: &Path, destination: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            fs::rename(temporary, destination)
        }
        #[cfg(windows)]
        {
            replace_atomically_windows(temporary, destination)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (temporary, destination);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "identity-safe replacement unsupported",
            ))
        }
    }

    fn cleanup(&mut self, temporary: Option<tempfile::TempPath>) -> CleanupStatus {
        temporary.map_or(CleanupStatus::NotNeeded, cleanup_temp_path)
    }
}

fn install_validated(input: &ValidatedInput) -> Result<(), DomainError> {
    let mut backend = ProductionInstallBackend;
    install_validated_with_backend(input, &mut backend)
}

fn install_validated_with_backend<B: InstallBackend>(
    input: &ValidatedInput,
    backend: &mut B,
) -> Result<(), DomainError> {
    use atomic_installer_model::{AtomicInstaller, Operation};

    #[cfg(windows)]
    {
        return Err(unsafe_write(
            &input.source_file,
            "identity-safe replacement unsupported",
            "Windows identity-safe replacement is not implemented with the available standard library APIs",
        ));
    }
    #[cfg(not(any(unix, windows)))]
    {
        return Err(unsafe_write(
            &input.source_file,
            "identity-safe replacement unsupported",
            "this platform cannot provide identity-safe replacement",
        ));
    }
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))
    ))]
    {
        return Err(unsafe_write(
            &input.source_file,
            "identity-safe replacement unsupported",
            "this Unix target lacks a standard no-follow open flag",
        ));
    }

    let parent = input.source_file.parent().ok_or_else(|| {
        unsafe_write(
            &input.source_file,
            "prepare parent",
            "source has no parent directory",
        )
    })?;
    let mut installer = AtomicInstaller::new();

    let prepared_parent = DirectoryIdentity::capture(parent).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::PrepareParent,
            &format!("operation failed: {error}"),
            CleanupStatus::NotNeeded,
            false,
        )
    })?;
    if prepared_parent != input.destination_parent {
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::PrepareParent,
            "destination parent identity changed",
            CleanupStatus::NotNeeded,
            false,
        ));
    }
    installer
        .complete(Operation::PrepareParent)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::PrepareParent,
                error,
                CleanupStatus::NotNeeded,
                false,
            )
        })?;

    recheck_inputs(input).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::RecheckInput,
            &format!("operation failed: {error}"),
            CleanupStatus::NotNeeded,
            false,
        )
    })?;
    installer
        .complete(Operation::RecheckInput)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::RecheckInput,
                error,
                CleanupStatus::NotNeeded,
                false,
            )
        })?;
    recheck_destination(input).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::RecheckDestination,
            &format!("operation failed: {error}"),
            CleanupStatus::NotNeeded,
            false,
        )
    })?;
    installer
        .complete(Operation::RecheckDestination)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::RecheckDestination,
                error,
                CleanupStatus::NotNeeded,
                false,
            )
        })?;
    recheck_parent(input, parent).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::RecheckParent,
            &format!("operation failed: {error}"),
            CleanupStatus::NotNeeded,
            false,
        )
    })?;
    installer
        .complete(Operation::RecheckParent)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::RecheckParent,
                error,
                CleanupStatus::NotNeeded,
                false,
            )
        })?;

    let mut temporary = backend.create_temp(parent).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::CreateTemp,
            &format!("operation failed: {error}"),
            CleanupStatus::NotNeeded,
            false,
        )
    })?;
    installer.complete(Operation::CreateTemp).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::CreateTemp,
            &error,
            CleanupStatus::NotNeeded,
            false,
        )
    })?;

    if let Err(error) = backend.write_complete(&mut temporary, &input.resolved) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::WriteReplacement,
            &format!("operation failed: {error}"),
            cleanup,
            false,
        ));
    }
    installer
        .complete(Operation::WriteReplacement)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::WriteReplacement,
                &error,
                CleanupStatus::PartialArtifactRemains,
                false,
            )
        })?;

    if let Err(error) = backend.copy_metadata(&temporary, &input.source_metadata) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::CopyMetadata,
            &format!("operation failed: {error}"),
            cleanup,
            false,
        ));
    }
    installer
        .complete(Operation::CopyMetadata)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::CopyMetadata,
                &error,
                CleanupStatus::PartialArtifactRemains,
                false,
            )
        })?;

    if let Err(error) = backend.sync_file(&temporary) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::SyncTemp,
            &format!("operation failed: {error}"),
            cleanup,
            false,
        ));
    }
    installer.complete(Operation::SyncTemp).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::SyncTemp,
            &error,
            CleanupStatus::PartialArtifactRemains,
            false,
        )
    })?;

    if let Err(error) = recheck_inputs(input) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::RecheckInput,
            &format!("operation failed: {error}"),
            cleanup,
            false,
        ));
    }
    if let Err(error) = installer.complete(Operation::RecheckInput) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::RecheckInput,
            error,
            cleanup,
            false,
        ));
    }
    if let Err(error) = recheck_destination(input) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::RecheckDestination,
            &format!("operation failed: {error}"),
            cleanup,
            false,
        ));
    }
    if let Err(error) = installer.complete(Operation::RecheckDestination) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::RecheckDestination,
            error,
            cleanup,
            false,
        ));
    }

    // These expected values are captured from the still-open file handle and
    // its directory after the complete write, metadata copy, and temp sync.
    // The final recheck below fresh-opens the pathname and rejects even a
    // same-byte inode replacement before the commit point.
    let expected_temp_identity = match FileIdentity::capture(temporary.as_file()) {
        Ok(identity) => identity,
        Err(error) => {
            let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
            return Err(installer_failure(
                &mut installer,
                input,
                Operation::RecheckTemp,
                &format!("operation failed: {error}"),
                cleanup,
                false,
            ));
        }
    };
    let expected_temp_parent = match DirectoryIdentity::capture(parent) {
        Ok(identity) => identity,
        Err(error) => {
            let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
            return Err(installer_failure(
                &mut installer,
                input,
                Operation::RecheckParent,
                &format!("operation failed: {error}"),
                cleanup,
                false,
            ));
        }
    };
    if expected_temp_parent != input.destination_parent {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::RecheckParent,
            "destination parent changed",
            cleanup,
            false,
        ));
    }
    installer
        .complete(Operation::RecheckTemp)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::RecheckTemp,
                &error,
                CleanupStatus::PartialArtifactRemains,
                false,
            )
        })?;
    installer
        .complete(Operation::RecheckParent)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::RecheckParent,
                &error,
                CleanupStatus::PartialArtifactRemains,
                false,
            )
        })?;

    // Sync the directory entry before the commit point.  This is deliberately
    // separate from the post-rename sync: a failure here is pre-commit and
    // therefore guarantees the destination remains unchanged.
    if let Err(error) = backend.sync_directory(parent) {
        let cleanup = backend.cleanup(Some(temporary.into_temp_path()));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::SyncParentBeforeCommit,
            &format!("operation failed: {error}"),
            cleanup,
            false,
        ));
    }
    installer
        .complete(Operation::SyncParentBeforeCommit)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::SyncParentBeforeCommit,
                &error,
                CleanupStatus::PartialArtifactRemains,
                false,
            )
        })?;

    // This is the single authoritative check immediately before the commit
    // point.  It fresh-opens every proposal input, the destination parent, and
    // the replacement pathname, comparing the temp identity and parent that
    // were captured from the completed/synced writer above.
    let temp_path = temporary.into_temp_path();
    if let Err(error) = final_recheck_before_rename(
        input,
        &temp_path,
        parent,
        &expected_temp_identity,
        &expected_temp_parent,
    ) {
        let cleanup = backend.cleanup(Some(temp_path));
        return Err(append_cleanup(error, cleanup));
    }
    // `final_recheck_before_rename` is the authoritative final check and runs
    // each recheck exactly once. Synchronize the model with that completed
    // check without replaying any transitions or filesystem operations.
    if let Err(error) = installer.mark_final_rechecked() {
        let cleanup = backend.cleanup(Some(temp_path));
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::RecheckTemp,
            error,
            cleanup,
            false,
        ));
    }
    if let Err(error) = backend.rename(&temp_path, &input.source_file) {
        let message = format!("rename failed: {error}");
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::PermissionDenied
                | io::ErrorKind::AlreadyExists
                | io::ErrorKind::InvalidInput
        ) {
            let cleanup = backend.cleanup(Some(temp_path));
            return Err(installer_failure(
                &mut installer,
                input,
                Operation::Rename,
                &message,
                cleanup,
                false,
            ));
        }
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::Rename,
            &message,
            CleanupStatus::PartialArtifactRemains,
            true,
        ));
    }
    installer.complete(Operation::Rename).map_err(|error| {
        installer_failure(
            &mut installer,
            input,
            Operation::Rename,
            error,
            CleanupStatus::NotNeeded,
            true,
        )
    })?;

    if let Err(error) = backend.sync_directory(parent) {
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::SyncParentAfterCommit,
            &format!("parent durability sync failed: {error}"),
            CleanupStatus::NotNeeded,
            false,
        ));
    }
    installer
        .complete(Operation::SyncParentAfterCommit)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::SyncParentAfterCommit,
                &error,
                CleanupStatus::NotNeeded,
                false,
            )
        })?;
    // The successful rename consumed the temporary pathname.  Cleanup remains
    // an explicit backend operation so injected post-commit failures are
    // reported as ambiguity rather than ordinary write failures.
    let cleanup = backend.cleanup(None);
    if cleanup != CleanupStatus::NotNeeded {
        return Err(installer_failure(
            &mut installer,
            input,
            Operation::CleanupTemp,
            "operation failed during cleanup",
            cleanup,
            false,
        ));
    }
    installer
        .complete(Operation::CleanupTemp)
        .map_err(|error| {
            installer_failure(
                &mut installer,
                input,
                Operation::CleanupTemp,
                &error,
                CleanupStatus::NotNeeded,
                false,
            )
        })?;
    Ok(())
}

fn installer_failure(
    installer: &mut atomic_installer_model::AtomicInstaller,
    input: &ValidatedInput,
    operation: atomic_installer_model::Operation,
    message: &str,
    cleanup: CleanupStatus,
    uncertain_rename: bool,
) -> DomainError {
    installer
        .failure(
            operation,
            &input.source_file,
            &input.workspace,
            cleanup,
            uncertain_rename,
        )
        .map_message(message)
}

trait DomainErrorMessage {
    fn map_message(self, message: &str) -> DomainError;
}

impl DomainErrorMessage for DomainError {
    fn map_message(self, message: &str) -> DomainError {
        match self {
            DomainError::UnsafeWrite { path, message: old } => DomainError::UnsafeWrite {
                path,
                message: old.replacen("operation failed", message, 1),
            },
            DomainError::InstallationAmbiguous {
                path,
                phase,
                workspace,
                may_have_committed,
                message: _old,
            } => DomainError::InstallationAmbiguous {
                path,
                phase,
                workspace,
                may_have_committed,
                message: message.to_owned(),
            },
            other => other,
        }
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    open_directory_handle(path)?.sync_all()
}

fn open_directory_handle(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        return OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path);
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

fn final_recheck_before_rename(
    input: &ValidatedInput,
    temporary: &Path,
    parent: &Path,
    expected_temp_identity: &FileIdentity,
    expected_temp_parent: &DirectoryIdentity,
) -> Result<(), DomainError> {
    // Keep these calls ordered and explicit: each maps a failure to the exact
    // pre-commit operation that made the refusal possible.
    recheck_inputs(input)?;
    recheck_destination(input)?;
    let temp_secure = read_secure_file(temporary, "replacement temp").map_err(|error| {
        unsafe_write_from_error(
            &input.source_file,
            "recheck temp",
            error,
            CleanupStatus::NotNeeded,
        )
    })?;
    if temp_secure.identity != *expected_temp_identity {
        return Err(unsafe_write(
            &input.source_file,
            "recheck temp",
            "replacement identity changed",
        ));
    }
    if temp_secure.bytes != input.resolved {
        return Err(unsafe_write(
            &input.source_file,
            "recheck temp",
            "replacement bytes changed",
        ));
    }
    if temp_secure.parent != *expected_temp_parent {
        return Err(unsafe_write(
            &input.source_file,
            "recheck temp",
            "replacement parent identity changed",
        ));
    }
    recheck_parent(input, parent)?;
    Ok(())
}

fn recheck_destination(input: &ValidatedInput) -> Result<(), DomainError> {
    let destination = read_secure_file(&input.source_file, "destination").map_err(|error| {
        unsafe_write_from_error(
            &input.source_file,
            "recheck destination",
            error,
            CleanupStatus::NotNeeded,
        )
    })?;
    let source_snapshot = input
        .snapshots
        .iter()
        .find(|snapshot| snapshot.path == input.source_file)
        .ok_or_else(|| {
            unsafe_write(
                &input.source_file,
                "recheck destination",
                "validated source snapshot is missing",
            )
        })?;
    if destination.identity != source_snapshot.identity
        || destination.parent != source_snapshot.parent
        || destination.bytes != input.source
    {
        return Err(unsafe_write(
            &input.source_file,
            "recheck destination",
            "destination identity or validated bytes changed",
        ));
    }
    Ok(())
}

fn recheck_parent(input: &ValidatedInput, parent: &Path) -> Result<(), DomainError> {
    let observed = DirectoryIdentity::capture(parent).map_err(|error| {
        unsafe_write_io(
            &input.source_file,
            "recheck parent",
            error,
            CleanupStatus::NotNeeded,
        )
    })?;
    if observed != input.destination_parent {
        return Err(unsafe_write(
            &input.source_file,
            "recheck parent",
            "destination parent identity changed",
        ));
    }
    Ok(())
}

fn recheck_inputs(input: &ValidatedInput) -> Result<(), DomainError> {
    for snapshot in &input.snapshots {
        let observed = read_secure_file(&snapshot.path, "validation input").map_err(|error| {
            unsafe_write_from_error(
                &snapshot.path,
                "recheck input",
                error,
                CleanupStatus::NotNeeded,
            )
        })?;
        if observed.identity != snapshot.identity || observed.parent != snapshot.parent {
            return Err(unsafe_write(
                &snapshot.path,
                "recheck input",
                "input identity changed",
            ));
        }
        if observed.bytes != snapshot.bytes {
            return Err(unsafe_write(
                &snapshot.path,
                "recheck input",
                "validated input bytes changed",
            ));
        }
    }
    let parent = input.source_file.parent().ok_or_else(|| {
        unsafe_write(&input.source_file, "recheck parent", "source has no parent")
    })?;
    if DirectoryIdentity::capture(parent).map_err(|error| {
        unsafe_write_io(
            &input.source_file,
            "recheck parent",
            error,
            CleanupStatus::NotNeeded,
        )
    })? != input.destination_parent
    {
        return Err(unsafe_write(
            &input.source_file,
            "recheck parent",
            "destination parent identity changed",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn replace_atomically_windows(temporary: &Path, destination: &Path) -> io::Result<()> {
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

fn open_secure_regular_file(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    reject_symlink_parents(parent)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0004_0000);
    }
    #[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0000_0100);
    }
    let link_metadata = fs::symlink_metadata(path)?;
    if link_metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file is a symlink",
        ));
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file is not regular",
        ));
    }
    Ok(file)
}

fn reject_symlink_parents(parent: &Path) -> io::Result<()> {
    let mut current = if parent.is_absolute() {
        PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
    } else {
        PathBuf::new()
    };
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => continue,
            Component::CurDir => continue,
            Component::ParentDir => {
                current.push(component.as_os_str());
            }
            Component::Normal(value) => {
                current.push(value);
                let metadata = fs::symlink_metadata(&current)?;
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "path traverses a non-directory or symlink",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn read_secure_file(path: &Path, label: &str) -> Result<SecureFile, DomainError> {
    let mut file = open_secure_regular_file(path).map_err(|error| path_error(path, error))?;
    let identity = FileIdentity::capture(&file).map_err(|error| path_error(path, error))?;
    let metadata = file.metadata().map_err(|error| path_error(path, error))?;
    let parent_path = path
        .parent()
        .ok_or_else(|| path_failure(path, "path has no parent directory"))?;
    let parent =
        DirectoryIdentity::capture(parent_path).map_err(|error| path_error(parent_path, error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| path_failure(path, format!("could not read {label}: {error}")))?;
    let observed = FileIdentity::capture(&file).map_err(|error| path_error(path, error))?;
    if identity != observed {
        return Err(path_failure(
            path,
            format!("{label} changed while it was being read"),
        ));
    }
    Ok(SecureFile {
        bytes,
        identity,
        metadata,
        parent,
    })
}

fn same_canonical(left: &Path, right: &Path) -> Result<bool, DomainError> {
    Ok(
        fs::canonicalize(left).map_err(|error| path_error(left, error))?
            == fs::canonicalize(right).map_err(|error| path_error(right, error))?,
    )
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
    reject_symlink_parents(parent).map_err(|error| path_error(parent, error))?;
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
        encode_path_for_output(manifest),
        encode_path_for_output(source),
        encode_path_for_output(resolved)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupStatus {
    /// No temporary artifact was created, so there was nothing to remove.
    NotNeeded,
    /// Cleanup was attempted and no temporary artifact remains.
    Completed,
    /// Cleanup was attempted but a partial temporary artifact remains.
    PartialArtifactRemains,
}

impl CleanupStatus {
    fn message(self) -> &'static str {
        match self {
            Self::NotNeeded => "cleanup not needed",
            Self::Completed => "cleanup completed",
            Self::PartialArtifactRemains => "cleanup failed; partial artifact remains",
        }
    }
}

fn unsafe_write(path: &Path, phase: &'static str, message: &str) -> DomainError {
    unsafe_write_with_cleanup(path, phase, message, CleanupStatus::NotNeeded)
}

fn unsafe_write_with_cleanup(
    path: &Path,
    phase: &'static str,
    message: &str,
    cleanup: CleanupStatus,
) -> DomainError {
    DomainError::UnsafeWrite {
        path: path.to_owned(),
        message: format!(
            "phase {phase}: {message}; destination is guaranteed unchanged; {}",
            cleanup.message()
        ),
    }
}

fn unsafe_write_io(
    path: &Path,
    phase: &'static str,
    error: io::Error,
    cleanup: CleanupStatus,
) -> DomainError {
    unsafe_write_with_cleanup(path, phase, &format!("operation failed: {error}"), cleanup)
}

fn unsafe_write_from_error(
    path: &Path,
    phase: &'static str,
    error: DomainError,
    cleanup: CleanupStatus,
) -> DomainError {
    unsafe_write_with_cleanup(path, phase, &format!("operation failed: {error}"), cleanup)
}

fn append_cleanup(error: DomainError, cleanup: CleanupStatus) -> DomainError {
    match error {
        DomainError::UnsafeWrite { path, mut message } => {
            message.push_str("; ");
            message.push_str(cleanup.message());
            DomainError::UnsafeWrite { path, message }
        }
        other => other,
    }
}

fn cleanup_temp(path: &Path) -> CleanupStatus {
    match fs::remove_file(path) {
        Ok(()) => CleanupStatus::Completed,
        Err(error) if error.kind() == io::ErrorKind::NotFound => CleanupStatus::Completed,
        Err(_) => CleanupStatus::PartialArtifactRemains,
    }
}

fn cleanup_named_temp(temporary: tempfile::NamedTempFile) -> CleanupStatus {
    cleanup_temp_path(temporary.into_temp_path())
}

fn cleanup_temp_path(temporary: tempfile::TempPath) -> CleanupStatus {
    let path: &Path = temporary.as_ref();
    match fs::remove_file(path) {
        Ok(()) => CleanupStatus::Completed,
        Err(error) if error.kind() == io::ErrorKind::NotFound => CleanupStatus::Completed,
        Err(_) => {
            // Keep the path owned by the caller after a failed removal so its
            // destructor cannot silently change the reported cleanup state.
            let _ = temporary.keep();
            CleanupStatus::PartialArtifactRemains
        }
    }
}

#[cfg(any())]
fn unsafe_write_with_temp(
    path: &Path,
    phase: &'static str,
    message: &str,
    temporary: tempfile::NamedTempFile,
) -> DomainError {
    unsafe_write_with_cleanup(path, phase, message, cleanup_named_temp(temporary))
}

#[cfg(any())]
fn unsafe_write_io_with_cleanup(
    path: &Path,
    phase: &'static str,
    error: io::Error,
    temporary: tempfile::NamedTempFile,
) -> DomainError {
    unsafe_write_with_temp(
        path,
        phase,
        &format!("operation failed: {error}"),
        temporary,
    )
}

#[cfg(any())]
fn unsafe_write_from_error_with_cleanup(
    path: &Path,
    phase: &'static str,
    error: DomainError,
    temporary: tempfile::NamedTempFile,
) -> DomainError {
    unsafe_write_with_temp(
        path,
        phase,
        &format!("operation failed: {error}"),
        temporary,
    )
}

/// Shared private commit state machine.  The production filesystem path and
/// the test failure-injection backend both drive this controller, so operation
/// ordering and the commit/ambiguity boundary cannot diverge.
mod atomic_installer_model {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Operation {
        CreateTemp,
        WriteReplacement,
        CopyMetadata,
        SyncTemp,
        PrepareParent,
        RecheckInput,
        RecheckDestination,
        RecheckParent,
        RecheckTemp,
        SyncParentBeforeCommit,
        Rename,
        SyncParentAfterCommit,
        CleanupTemp,
    }

    impl Operation {
        pub(super) fn phase(self) -> &'static str {
            match self {
                Self::CreateTemp => "create temp",
                Self::WriteReplacement => "write replacement",
                Self::CopyMetadata => "copy metadata",
                Self::SyncTemp => "sync temp",
                Self::PrepareParent => "prepare parent",
                Self::RecheckInput => "recheck input",
                Self::RecheckDestination => "recheck destination",
                Self::RecheckParent => "recheck parent",
                Self::RecheckTemp => "recheck temp",
                Self::SyncParentBeforeCommit => "sync parent before commit",
                Self::Rename => "rename",
                Self::SyncParentAfterCommit => "sync parent after commit",
                Self::CleanupTemp => "cleanup temp",
            }
        }

        pub(super) fn post_commit(self) -> bool {
            matches!(self, Self::SyncParentAfterCommit | Self::CleanupTemp)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum InstallerState {
        Ready,
        TempCreated,
        ReplacementWritten,
        MetadataCopied,
        TempSynced,
        ParentPrepared,
        InputsRechecked,
        DestinationRechecked,
        ParentRechecked,
        TempRechecked,
        ParentSyncedBeforeCommit,
        Committed,
        CommitUncertain,
        Finished,
    }

    impl InstallerState {
        pub(super) fn committed_or_uncertain(self) -> bool {
            matches!(
                self,
                Self::Committed | Self::CommitUncertain | Self::Finished
            )
        }
    }

    #[derive(Debug)]
    pub(super) struct AtomicInstaller {
        state: InstallerState,
        temp_created: bool,
    }

    impl AtomicInstaller {
        pub(super) fn new() -> Self {
            Self {
                state: InstallerState::Ready,
                temp_created: false,
            }
        }

        #[cfg(test)]
        pub(super) fn state(&self) -> InstallerState {
            self.state
        }

        pub(super) fn temp_created(&self) -> bool {
            self.temp_created
        }

        pub(super) fn mark_final_rechecked(&mut self) -> Result<(), &'static str> {
            if self.state != InstallerState::ParentSyncedBeforeCommit {
                return Err("final recheck completed from an invalid installer state");
            }
            self.state = InstallerState::TempRechecked;
            Ok(())
        }

        pub(super) fn complete(&mut self, operation: Operation) -> Result<(), &'static str> {
            let next = match (self.state, operation) {
                (InstallerState::Ready, Operation::PrepareParent) => InstallerState::ParentPrepared,
                (InstallerState::ParentPrepared, Operation::RecheckInput) => {
                    InstallerState::InputsRechecked
                }
                (InstallerState::InputsRechecked, Operation::RecheckDestination) => {
                    InstallerState::DestinationRechecked
                }
                (InstallerState::DestinationRechecked, Operation::RecheckParent) => {
                    InstallerState::ParentRechecked
                }
                (InstallerState::ParentRechecked, Operation::CreateTemp) => {
                    self.temp_created = true;
                    InstallerState::TempCreated
                }
                (InstallerState::TempCreated, Operation::WriteReplacement) => {
                    InstallerState::ReplacementWritten
                }
                (InstallerState::ReplacementWritten, Operation::CopyMetadata) => {
                    InstallerState::MetadataCopied
                }
                (InstallerState::MetadataCopied, Operation::SyncTemp) => InstallerState::TempSynced,
                (InstallerState::TempSynced, Operation::RecheckInput) => {
                    InstallerState::InputsRechecked
                }
                (InstallerState::DestinationRechecked, Operation::RecheckTemp) => {
                    InstallerState::TempRechecked
                }
                (InstallerState::TempRechecked, Operation::RecheckParent) => {
                    InstallerState::ParentRechecked
                }
                (InstallerState::ParentRechecked, Operation::SyncParentBeforeCommit) => {
                    InstallerState::ParentSyncedBeforeCommit
                }
                (InstallerState::ParentSyncedBeforeCommit, Operation::RecheckInput) => {
                    InstallerState::InputsRechecked
                }
                (InstallerState::ParentRechecked, Operation::RecheckTemp) => {
                    InstallerState::TempRechecked
                }
                (InstallerState::TempRechecked, Operation::Rename) => InstallerState::Committed,
                (InstallerState::Committed, Operation::SyncParentAfterCommit) => {
                    InstallerState::Committed
                }
                (InstallerState::Committed, Operation::CleanupTemp) => InstallerState::Finished,
                _ => return Err("invalid atomic installer operation transition"),
            };
            self.state = next;
            Ok(())
        }

        pub(super) fn failure(
            &mut self,
            operation: Operation,
            path: &Path,
            workspace: &Path,
            cleanup: CleanupStatus,
            uncertain_rename: bool,
        ) -> DomainError {
            if operation.post_commit() || uncertain_rename || self.state.committed_or_uncertain() {
                if uncertain_rename {
                    self.state = InstallerState::CommitUncertain;
                }
                return DomainError::InstallationAmbiguous {
                    path: path.to_owned(),
                    phase: operation.phase(),
                    workspace: workspace.to_owned(),
                    may_have_committed: true,
                    message: "replacement status is uncertain; inspect the source and workspace"
                        .into(),
                };
            }
            unsafe_write_with_cleanup(
                path,
                operation.phase(),
                "operation failed",
                if self.temp_created() {
                    cleanup
                } else {
                    CleanupStatus::NotNeeded
                },
            )
        }
    }
}

#[cfg(test)]
mod atomic_installer_model_tests {
    use super::atomic_installer_model::{AtomicInstaller, InstallerState, Operation};

    #[test]
    fn state_machine_reaches_finished_after_all_operations() {
        let mut installer = AtomicInstaller::new();
        for operation in [
            Operation::PrepareParent,
            Operation::RecheckInput,
            Operation::RecheckDestination,
            Operation::RecheckParent,
            Operation::CreateTemp,
            Operation::WriteReplacement,
            Operation::CopyMetadata,
            Operation::SyncTemp,
            Operation::RecheckInput,
            Operation::RecheckDestination,
            Operation::RecheckTemp,
            Operation::RecheckParent,
            Operation::SyncParentBeforeCommit,
            Operation::RecheckInput,
            Operation::RecheckDestination,
            Operation::RecheckParent,
            Operation::RecheckTemp,
            Operation::Rename,
            Operation::SyncParentAfterCommit,
            Operation::CleanupTemp,
        ] {
            installer.complete(operation).unwrap();
        }
        assert_eq!(installer.state(), InstallerState::Finished);
        assert!(installer.state().committed_or_uncertain());
    }

    #[test]
    fn post_commit_operations_are_classified_as_ambiguous() {
        let mut installer = AtomicInstaller::new();
        for operation in [
            Operation::PrepareParent,
            Operation::RecheckInput,
            Operation::RecheckDestination,
            Operation::RecheckParent,
            Operation::CreateTemp,
            Operation::WriteReplacement,
            Operation::CopyMetadata,
            Operation::SyncTemp,
            Operation::RecheckInput,
            Operation::RecheckDestination,
            Operation::RecheckTemp,
            Operation::RecheckParent,
            Operation::SyncParentBeforeCommit,
            Operation::RecheckInput,
            Operation::RecheckDestination,
            Operation::RecheckParent,
            Operation::RecheckTemp,
            Operation::Rename,
        ] {
            installer.complete(operation).unwrap();
        }
        for operation in [Operation::SyncParentAfterCommit, Operation::CleanupTemp] {
            let error = installer.failure(
                operation,
                std::path::Path::new("source"),
                std::path::Path::new("workspace"),
                super::CleanupStatus::NotNeeded,
                false,
            );
            assert!(matches!(
                error,
                super::DomainError::InstallationAmbiguous { .. }
            ));
            assert!(
                error
                    .to_string()
                    .contains("replacement may already be present")
            );
            assert_eq!(installer.state(), InstallerState::Committed);
        }
    }
}

#[cfg(all(
    test,
    unix,
    any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )
))]
mod install_backend_tests {
    use super::atomic_installer_model::Operation;
    use super::*;

    struct TestInstallBackend {
        fail_at: Option<Operation>,
        cleanup_result: CleanupStatus,
        uncertain_rename: bool,
        directory_syncs: usize,
    }

    impl TestInstallBackend {
        fn new(fail_at: Option<Operation>) -> Self {
            Self {
                fail_at,
                cleanup_result: CleanupStatus::Completed,
                uncertain_rename: false,
                directory_syncs: 0,
            }
        }

        fn fails(&self, operation: Operation) -> bool {
            self.fail_at == Some(operation)
        }

        fn injected_error(operation: Operation) -> io::Error {
            io::Error::new(
                io::ErrorKind::Other,
                format!("injected {} failure", operation.phase()),
            )
        }
    }

    impl InstallBackend for TestInstallBackend {
        fn create_temp(&mut self, parent: &Path) -> io::Result<tempfile::NamedTempFile> {
            if self.fails(Operation::CreateTemp) {
                return Err(Self::injected_error(Operation::CreateTemp));
            }
            ProductionInstallBackend.create_temp(parent)
        }

        fn write_complete(
            &mut self,
            temporary: &mut tempfile::NamedTempFile,
            bytes: &[u8],
        ) -> io::Result<()> {
            if self.fails(Operation::WriteReplacement) {
                return Err(Self::injected_error(Operation::WriteReplacement));
            }
            ProductionInstallBackend.write_complete(temporary, bytes)
        }

        fn copy_metadata(
            &mut self,
            temporary: &tempfile::NamedTempFile,
            metadata: &Metadata,
        ) -> io::Result<()> {
            if self.fails(Operation::CopyMetadata) {
                return Err(Self::injected_error(Operation::CopyMetadata));
            }
            ProductionInstallBackend.copy_metadata(temporary, metadata)
        }

        fn sync_file(&mut self, temporary: &tempfile::NamedTempFile) -> io::Result<()> {
            if self.fails(Operation::SyncTemp) {
                return Err(Self::injected_error(Operation::SyncTemp));
            }
            ProductionInstallBackend.sync_file(temporary)
        }

        fn sync_directory(&mut self, parent: &Path) -> io::Result<()> {
            self.directory_syncs += 1;
            let operation = if self.directory_syncs == 1 {
                Operation::SyncParentBeforeCommit
            } else {
                Operation::SyncParentAfterCommit
            };
            if self.fails(operation) {
                return Err(Self::injected_error(operation));
            }
            let _ = parent;
            Ok(())
        }

        fn rename(&mut self, temporary: &Path, destination: &Path) -> io::Result<()> {
            if self.fails(Operation::Rename) {
                if self.uncertain_rename {
                    fs::rename(temporary, destination)?;
                    return Err(Self::injected_error(Operation::Rename));
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected known-safe rename failure",
                ));
            }
            fs::rename(temporary, destination)
        }

        fn cleanup(&mut self, temporary: Option<tempfile::TempPath>) -> CleanupStatus {
            match temporary {
                Some(temporary) if self.cleanup_result == CleanupStatus::PartialArtifactRemains => {
                    let _ = temporary.keep();
                    CleanupStatus::PartialArtifactRemains
                }
                Some(temporary) => cleanup_temp_path(temporary),
                None if self.fails(Operation::CleanupTemp) => self.cleanup_result,
                None => CleanupStatus::NotNeeded,
            }
        }
    }

    fn fixture() -> (tempfile::TempDir, ValidatedInput, Vec<u8>) {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source");
        fs::write(&source_path, b"before").unwrap();
        let source_file = fs::canonicalize(&source_path).unwrap();
        let secure = read_secure_file(&source_file, "source").unwrap();
        let source = secure.bytes.clone();
        let snapshot = InputSnapshot {
            path: source_file.clone(),
            bytes: secure.bytes.clone(),
            identity: secure.identity,
            parent: secure.parent,
        };
        let destination_parent = DirectoryIdentity::capture(directory.path()).unwrap();
        let input = ValidatedInput {
            source_file,
            source,
            resolved: b"after".to_vec(),
            plan: ApplyPlan { hunks: Vec::new() },
            snapshots: vec![snapshot],
            workspace: directory.path().to_owned(),
            destination_parent,
            source_metadata: secure.metadata,
        };
        let original = input.source.clone();
        (directory, input, original)
    }

    #[test]
    fn install_backend_success_reaches_rename_and_changes_source() {
        let (_directory, input, original) = fixture();
        let mut backend = TestInstallBackend::new(None);
        install_validated_with_backend(&input, &mut backend).unwrap();
        assert_ne!(fs::read(&input.source_file).unwrap(), original);
        assert_eq!(fs::read(&input.source_file).unwrap(), input.resolved);
    }

    #[test]
    fn install_backend_precommit_failure_matrix_preserves_source() {
        for operation in [
            Operation::CreateTemp,
            Operation::WriteReplacement,
            Operation::CopyMetadata,
            Operation::SyncTemp,
            Operation::SyncParentBeforeCommit,
            Operation::Rename,
        ] {
            let (_directory, input, original) = fixture();
            let mut backend = TestInstallBackend::new(Some(operation));
            let error = install_validated_with_backend(&input, &mut backend).unwrap_err();
            let rendered = error.to_string();
            assert!(matches!(error, DomainError::UnsafeWrite { .. }));
            assert!(rendered.contains(operation.phase()), "{rendered}");
            assert!(rendered.contains("destination is guaranteed unchanged"));
            let expected_cleanup = if operation == Operation::CreateTemp {
                CleanupStatus::NotNeeded
            } else {
                CleanupStatus::Completed
            };
            assert!(rendered.contains(expected_cleanup.message()), "{rendered}");
            assert_eq!(fs::read(&input.source_file).unwrap(), original);
        }
    }

    #[test]
    fn install_backend_precommit_cleanup_failure_reports_partial_artifact() {
        let (_directory, input, original) = fixture();
        let mut backend = TestInstallBackend::new(Some(Operation::WriteReplacement));
        backend.cleanup_result = CleanupStatus::PartialArtifactRemains;
        let error = install_validated_with_backend(&input, &mut backend).unwrap_err();
        let rendered = error.to_string();
        assert!(matches!(error, DomainError::UnsafeWrite { .. }));
        assert!(rendered.contains("phase write replacement"));
        assert!(rendered.contains(CleanupStatus::PartialArtifactRemains.message()));
        assert!(rendered.contains("destination is guaranteed unchanged"));
        assert_eq!(fs::read(&input.source_file).unwrap(), original);
    }

    #[test]
    fn install_backend_postcommit_failures_report_ambiguity_after_rename() {
        for operation in [Operation::SyncParentAfterCommit, Operation::CleanupTemp] {
            let (_directory, input, original) = fixture();
            let mut backend = TestInstallBackend::new(Some(operation));
            if operation == Operation::CleanupTemp {
                backend.cleanup_result = CleanupStatus::PartialArtifactRemains;
            }
            let error = install_validated_with_backend(&input, &mut backend).unwrap_err();
            let rendered = error.to_string();
            assert!(matches!(
                error,
                DomainError::InstallationAmbiguous {
                    may_have_committed: true,
                    ..
                }
            ));
            assert!(
                rendered.contains("replacement may already be present"),
                "{rendered}"
            );
            assert!(!rendered.contains("destination is guaranteed unchanged"));
            assert_ne!(fs::read(&input.source_file).unwrap(), original);
            assert_eq!(fs::read(&input.source_file).unwrap(), input.resolved);
        }
    }

    #[test]
    fn install_backend_uncertain_rename_reports_ambiguity_after_replacement() {
        let (_directory, input, original) = fixture();
        let mut backend = TestInstallBackend::new(Some(Operation::Rename));
        backend.uncertain_rename = true;
        let error = install_validated_with_backend(&input, &mut backend).unwrap_err();
        let rendered = error.to_string();
        assert!(matches!(
            error,
            DomainError::InstallationAmbiguous {
                may_have_committed: true,
                ..
            }
        ));
        assert!(rendered.contains("replacement may already be present"));
        assert_ne!(fs::read(&input.source_file).unwrap(), original);
        assert_eq!(fs::read(&input.source_file).unwrap(), input.resolved);
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

    #[test]
    fn cleanup_status_distinguishes_all_outcomes() {
        let path = PathBuf::from("does-not-exist");
        assert_eq!(cleanup_temp(&path), CleanupStatus::Completed);
        assert_eq!(CleanupStatus::NotNeeded.message(), "cleanup not needed");
        assert_eq!(CleanupStatus::Completed.message(), "cleanup completed");
        assert_eq!(
            CleanupStatus::PartialArtifactRemains.message(),
            "cleanup failed; partial artifact remains"
        );

        let directory = tempfile::tempdir().unwrap();
        let non_file = directory.path().join("not-a-file");
        fs::create_dir(&non_file).unwrap();
        assert_eq!(
            cleanup_temp(&non_file),
            CleanupStatus::PartialArtifactRemains
        );
    }

    #[test]
    fn temp_cleanup_removes_created_artifact_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = Builder::new().tempfile_in(directory.path()).unwrap();
        let path = temporary.path().to_owned();
        assert!(path.exists());
        assert_eq!(cleanup_named_temp(temporary), CleanupStatus::Completed);
        assert!(!path.exists());
    }

    #[test]
    fn pre_commit_error_display_contains_phase_path_and_cleanup_state() {
        let path = PathBuf::from("source");
        let error = unsafe_write(&path, "create temp", "cannot create sibling");
        let rendered = error.to_string();
        assert!(rendered.contains("unsafe write refused for `source`"));
        assert!(rendered.contains("phase create temp"));
        assert!(rendered.contains("cleanup not needed"));
        assert!(rendered.contains("destination is guaranteed unchanged"));

        for (cleanup, expected) in [
            (CleanupStatus::NotNeeded, "cleanup not needed"),
            (CleanupStatus::Completed, "cleanup completed"),
            (
                CleanupStatus::PartialArtifactRemains,
                "cleanup failed; partial artifact remains",
            ),
        ] {
            let error = unsafe_write_with_cleanup(&path, "sync temp", "sync failed", cleanup);
            let rendered = error.to_string();
            assert!(rendered.contains("phase sync temp"));
            assert!(rendered.contains(expected));
            assert!(rendered.contains("destination is guaranteed unchanged"));
        }
    }

    #[test]
    fn file_identity_equal_for_same_handle_and_differs_after_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let directory_path = fs::canonicalize(directory.path()).unwrap();
        let path = directory_path.join("source");
        fs::write(&path, b"before").unwrap();
        let file = open_secure_regular_file(&path).unwrap();
        let first = FileIdentity::capture(&file).unwrap();
        let second = FileIdentity::capture(&file).unwrap();
        assert_eq!(first, second);

        #[cfg(unix)]
        {
            let replacement = directory_path.join("replacement");
            fs::write(&replacement, b"before").unwrap();
            fs::rename(replacement, &path).unwrap();
            let observed =
                FileIdentity::capture(&open_secure_regular_file(&path).unwrap()).unwrap();
            assert_ne!(first, observed);
        }
    }

    #[test]
    fn installation_ambiguity_display_never_claims_unchanged_or_success() {
        let error = DomainError::InstallationAmbiguous {
            path: PathBuf::from("source\n%\u{1}"),
            phase: "sync parent after commit",
            workspace: PathBuf::from("workspace\n%\u{1}"),
            may_have_committed: true,
            message: "directory sync failed".into(),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("installation ambiguous for `source%0A%25%01`"));
        assert!(rendered.contains("during sync parent after commit"));
        assert!(rendered.contains("workspace `workspace%0A%25%01`"));
        assert!(rendered.contains("replacement may already be present: true"));
        assert!(rendered.contains("inspect the source and workspace"));
        assert!(!rendered.contains("destination is guaranteed unchanged"));
        assert!(!rendered.contains("successfully installed"));
    }

    #[test]
    fn secure_recheck_accepts_unchanged_input_and_rejects_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let directory_path = fs::canonicalize(directory.path()).unwrap();
        let path = directory_path.join("source");
        fs::write(&path, b"source").unwrap();
        let secure = read_secure_file(&path, "source").unwrap();
        let snapshot = InputSnapshot {
            path: path.clone(),
            bytes: secure.bytes.clone(),
            identity: secure.identity.clone(),
            parent: secure.parent.clone(),
        };
        let input = ValidatedInput {
            source_file: path.clone(),
            source: secure.bytes,
            resolved: b"resolved".to_vec(),
            plan: ApplyPlan { hunks: Vec::new() },
            snapshots: vec![snapshot],
            workspace: directory_path.clone(),
            destination_parent: DirectoryIdentity::capture(&directory_path).unwrap(),
            source_metadata: secure.metadata,
        };
        assert!(recheck_inputs(&input).is_ok());

        #[cfg(unix)]
        {
            let replacement = directory_path.join("replacement");
            fs::write(&replacement, b"source").unwrap();
            fs::rename(replacement, &path).unwrap();
            let error = recheck_inputs(&input).unwrap_err().to_string();
            assert!(error.contains("phase recheck input"));
            assert!(error.contains("cleanup not needed"));
        }
    }

    #[test]
    fn secure_recheck_rejects_in_place_same_length_same_mtime_edit() {
        let directory = tempfile::tempdir().unwrap();
        let directory_path = fs::canonicalize(directory.path()).unwrap();
        let path = directory_path.join("source");
        fs::write(&path, b"source").unwrap();
        let secure = read_secure_file(&path, "source").unwrap();
        let snapshot = InputSnapshot {
            path: path.clone(),
            bytes: secure.bytes.clone(),
            identity: secure.identity.clone(),
            parent: secure.parent.clone(),
        };
        let input = ValidatedInput {
            source_file: path.clone(),
            source: secure.bytes.clone(),
            resolved: b"target".to_vec(),
            plan: ApplyPlan { hunks: Vec::new() },
            snapshots: vec![snapshot],
            workspace: directory_path.clone(),
            destination_parent: DirectoryIdentity::capture(&directory_path).unwrap(),
            source_metadata: secure.metadata.clone(),
        };
        let access_time = FileTime::from_last_access_time(&secure.metadata);
        let modification_time = FileTime::from_last_modification_time(&secure.metadata);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"target").unwrap();
        set_file_handle_times(&file, Some(access_time), Some(modification_time)).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let observed = read_secure_file(&path, "source").unwrap();
        assert_eq!(observed.identity, secure.identity);
        assert_eq!(observed.bytes, b"target");
        let error = recheck_inputs(&input).unwrap_err().to_string();
        assert!(error.contains("phase recheck input"));
        assert!(error.contains("validated input bytes changed"));
        assert!(error.contains("destination is guaranteed unchanged"));
    }
}
