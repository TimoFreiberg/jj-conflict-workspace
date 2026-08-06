use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::UNIX_EPOCH;

use crate::cli::PrepareOptions;
use crate::core::{materialize_unresolved, parse_snapshot, region_seed, region_trailing_eol};
use crate::domain::{
    MANIFEST_SCHEMA_VERSION, Manifest, ManifestRegion, ManifestTerm, Sha256Digest, SourceIdentity,
};
use crate::error::DomainError;
use crate::path_output::encode_path_for_output;
use crate::repository_root::resolve_repository_source;

const JJ_PROGRAM: &str = "jj";
const JJ_DIAGNOSTIC_LIMIT: usize = 65_536;
const JJ_CONFIG: &str = "ui.conflict-marker-style=snapshot";
const WORKSPACE_PREFIX: &str = "jcw-";

#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn capture(file: &File, _metadata: &fs::Metadata) -> io::Result<Self> {
        // Always obtain the fields from the already-open handle. In particular, do
        // not turn a metadata failure or a pre-epoch timestamp into a sentinel:
        // that could make a same-byte replacement look unchanged.
        let metadata = file.metadata()?;
        let modified = metadata.modified()?;
        let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file modification time predates the Unix epoch: {error}"),
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

#[derive(Debug)]
struct SourceContext {
    canonical_source: PathBuf,
    repository_root: PathBuf,
    repository_relative: PathBuf,
    original: Vec<u8>,
    identity: FileIdentity,
}

/// Prepare a source file into a private, persistent workspace.
///
/// The child process invocation is intentionally kept here, at the imperative
/// boundary. The parser and unresolved-seed materializer remain pure functions.
pub fn run(options: &PrepareOptions) -> Result<PathBuf, DomainError> {
    let mut writer = RealWorkspaceWriter;
    run_with_writer_impl(options, &mut writer, invoke_jj)
}

fn run_with_writer_impl<W, F>(
    options: &PrepareOptions,
    writer: &mut W,
    invoke_snapshot: F,
) -> Result<PathBuf, DomainError>
where
    W: WorkspaceWriter,
    F: FnOnce(&SourceContext) -> Result<Vec<u8>, DomainError>,
{
    let source = validate_source(&options.file)?;
    let snapshot = invoke_snapshot(&source)?;
    let (current_bytes, current_identity) = read_source_securely(&source.canonical_source)
        .map_err(|error| path_error(&source.canonical_source, error))?;
    if current_bytes != source.original || current_identity != source.identity {
        return Err(DomainError::SourceChanged {
            path: source.canonical_source.clone(),
            phase: "after-jj",
            expected: source.identity.to_string(),
            observed: current_identity.to_string(),
        });
    }
    let document = parse_snapshot(&snapshot).map_err(|error| match error {
        DomainError::NoConflictFound { .. } => DomainError::NoConflictFound {
            path: source.repository_relative.clone(),
        },
        other => describe_parse_failure(&source.repository_relative, &snapshot, other),
    })?;
    let resolved = materialize_unresolved(&document).map_err(|error| {
        DomainError::invalid(format!(
            "JJ snapshot output could not be materialized: {}",
            render_without_prefix(&error)
        ))
    })?;

    let manifest = make_manifest(&source, &document)?;
    let workspace = match writer.create_workspace(options.output_dir.as_deref()) {
        Ok(workspace) => workspace,
        Err(mut error) => {
            // A writer can fail after creating the workspace directory (for
            // example, while restricting its permissions).  In that case the
            // caller still owns cleanup and must report its result.
            if let Some(workspace) = error.partial_workspace.take() {
                return Err(cleanup_failed_workspace(writer, workspace, error));
            }
            return Err(workspace_failure_error(error));
        }
    };
    match write_workspace(
        writer,
        &workspace,
        &source.original,
        &document,
        &resolved,
        &manifest,
    ) {
        Ok(()) => Ok(workspace),
        Err(error) => Err(cleanup_failed_workspace(writer, workspace, error)),
    }
}

/// Test-only prepare boundary. The injected snapshot runner avoids changing PATH
/// or starting a child process, while the writer remains private to this module.
#[cfg(test)]
fn run_with_writer<W, F>(
    options: &PrepareOptions,
    writer: &mut W,
    invoke_snapshot: F,
) -> Result<PathBuf, DomainError>
where
    W: WorkspaceWriter,
    F: FnOnce(&SourceContext) -> Result<Vec<u8>, DomainError>,
{
    run_with_writer_impl(options, writer, invoke_snapshot)
}

fn validate_source(path: &Path) -> Result<SourceContext, DomainError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if !metadata.file_type().is_file() {
        return Err(DomainError::PathUnavailable {
            path: path.to_owned(),
            message: "the source must be a regular file, not a directory or symlink".into(),
        });
    }

    let resolved = resolve_repository_source(path).map_err(|error| path_error(path, error))?;
    validate_relative_path(&resolved.repository_relative, path)?;
    let (original, identity) = read_source_securely(&resolved.canonical_source)
        .map_err(|error| path_error(path, error))?;

    Ok(SourceContext {
        canonical_source: resolved.canonical_source,
        repository_root: resolved.repository_root,
        repository_relative: resolved.repository_relative,
        original,
        identity,
    })
}

fn read_source_securely(path: &Path) -> io::Result<(Vec<u8>, FileIdentity)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(all(unix, target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0004_0000);
    }
    #[cfg(all(unix, target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0004_0000);
    }
    #[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0000_0100);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    let identity = FileIdentity::capture(&file, &metadata)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let observed = FileIdentity::capture(&file, &file.metadata()?)?;
    if observed != identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source changed while it was being read",
        ));
    }
    Ok((bytes, identity))
}

/// Render a snapshot parse failure so the user can act on it.
///
/// The markerless case is reported by the caller as [`DomainError::NoConflictFound`]
/// with the requested path. Structural errors keep their parser message but
/// replace the raw byte offset with a line number and the offending line's
/// content.
fn describe_parse_failure(relative: &Path, snapshot: &[u8], error: DomainError) -> DomainError {
    let message = match error {
        DomainError::InvalidInput {
            message,
            region_index,
            byte_offset,
            range,
        } => {
            let mut rendered = format!(
                "JJ snapshot output for `{}` could not be parsed: {message}",
                encode_path_for_output(relative)
            );
            if let Some(index) = region_index {
                rendered.push_str(&format!(" (region {index})"));
            }
            match byte_offset.and_then(|offset| line_context(snapshot, offset)) {
                Some((line_number, content)) => {
                    rendered.push_str(&format!(" at line {line_number}: `{content}`"));
                }
                None if byte_offset.is_some() => {
                    rendered.push_str(" at end of file");
                }
                None => {}
            }
            if let Some((start, end)) = range {
                rendered.push_str(&format!(" at byte range [{start}, {end})"));
            }
            rendered
        }
        other => format!(
            "JJ snapshot output for `{}` could not be parsed: {other}",
            encode_path_for_output(relative)
        ),
    };
    DomainError::invalid(message)
}

/// Return the 1-based line number and printable content of the line that
/// contains `offset` in `snapshot`. Offsets at or past the last line end map
/// to the final line; the caller decides how to phrase that case.
fn line_context(snapshot: &[u8], offset: usize) -> Option<(usize, String)> {
    if offset > snapshot.len() {
        return None;
    }
    let mut line_number = 1usize;
    let mut line_start = 0usize;
    while line_start < snapshot.len() {
        let relative_end = snapshot[line_start..]
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(snapshot.len() - line_start);
        let line_end = line_start + relative_end;
        let content_end = if line_end > line_start && snapshot[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        if offset <= line_end {
            let mut content =
                String::from_utf8_lossy(&snapshot[line_start..content_end]).into_owned();
            if content.chars().count() > 80 {
                content = content.chars().take(80).collect();
            }
            return Some((line_number, content));
        }
        line_number += 1;
        line_start = line_end + 1;
    }
    None
}

/// Render a domain error without the `invalid input:` prefix so it can be
/// embedded in a higher-level message without doubling prefixes.
fn render_without_prefix(error: &DomainError) -> String {
    match error {
        DomainError::InvalidInput {
            message,
            region_index,
            byte_offset,
            range,
        } => {
            let mut rendered = message.clone();
            if let Some(index) = region_index {
                rendered.push_str(&format!(" (region {index})"));
            }
            if let Some(offset) = byte_offset {
                rendered.push_str(&format!(" at byte offset {offset}"));
            }
            if let Some((start, end)) = range {
                rendered.push_str(&format!(" at byte range [{start}, {end})"));
            }
            rendered
        }
        other => other.to_string(),
    }
}

fn validate_relative_path(path: &Path, original: &Path) -> Result<(), DomainError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.has_root()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(DomainError::PathUnavailable {
            path: original.to_owned(),
            message: "the source does not have a safe repository-relative path".into(),
        });
    }
    Ok(())
}

fn invoke_jj(source: &SourceContext) -> Result<Vec<u8>, DomainError> {
    let mut command = ProcessCommand::new(JJ_PROGRAM);
    command
        .arg("--no-pager")
        .arg("--config")
        .arg(JJ_CONFIG)
        .arg("file")
        .arg("show")
        .arg("--revision")
        .arg("@")
        .arg("--")
        .arg(&source.repository_relative)
        .current_dir(&source.repository_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let description = format_command(&source.repository_relative);
    invoke_process(&mut command, &description)
}

fn invoke_process(command: &mut ProcessCommand, description: &str) -> Result<Vec<u8>, DomainError> {
    let mut child = command
        .spawn()
        .map_err(|error| DomainError::ExternalCommand {
            command: description.to_owned(),
            status: None,
            stderr: format!("could not start JJ: {error}"),
        })?;
    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_bounded_pipe(child.stderr.take(), JJ_DIAGNOSTIC_LIMIT);
    let status = child.wait().map_err(|error| DomainError::ExternalCommand {
        command: description.to_owned(),
        status: None,
        stderr: format!("could not wait for JJ: {error}"),
    })?;
    let stdout = join_pipe(stdout_reader, description)?;
    let stderr = join_bounded_pipe(stderr_reader, description)?;
    if !status.success() {
        return Err(DomainError::ExternalCommand {
            command: description.to_owned(),
            status: status.code(),
            stderr: command_diagnostics(&stderr),
        });
    }
    Ok(stdout)
}

fn drain_pipe<R>(reader: Option<R>) -> thread::JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut reader) = reader {
            reader.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    })
}

fn drain_bounded_pipe<R>(
    reader: Option<R>,
    limit: usize,
) -> thread::JoinHandle<io::Result<BoundedBytes>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut retained = Vec::new();
        let mut total = 0usize;
        let mut buffer = [0u8; 8192];
        if let Some(mut reader) = reader {
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                total = total.saturating_add(count);
                let remaining = limit.saturating_sub(retained.len());
                if remaining > 0 {
                    retained.extend_from_slice(&buffer[..count.min(remaining)]);
                }
            }
        }
        Ok(BoundedBytes {
            bytes: retained,
            total,
        })
    })
}

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    total: usize,
}

fn join_pipe(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    command: &str,
) -> Result<Vec<u8>, DomainError> {
    match reader.join() {
        Ok(result) => result.map_err(|error| DomainError::ExternalCommand {
            command: command.to_owned(),
            status: None,
            stderr: format!("could not drain JJ output: {error}"),
        }),
        Err(_) => Err(DomainError::ExternalCommand {
            command: command.to_owned(),
            status: None,
            stderr: "could not drain JJ output: reader thread panicked".into(),
        }),
    }
}

fn join_bounded_pipe(
    reader: thread::JoinHandle<io::Result<BoundedBytes>>,
    command: &str,
) -> Result<Vec<u8>, DomainError> {
    match reader.join() {
        Ok(result) => result
            .map(|output| {
                render_bounded_jj_stderr(&output.bytes, output.total > JJ_DIAGNOSTIC_LIMIT)
                    .into_bytes()
            })
            .map_err(|error| DomainError::ExternalCommand {
                command: command.to_owned(),
                status: None,
                stderr: format!("could not drain JJ output: {error}"),
            }),
        Err(_) => Err(DomainError::ExternalCommand {
            command: command.to_owned(),
            status: None,
            stderr: "could not drain JJ output: reader thread panicked".into(),
        }),
    }
}

fn format_command(path: &Path) -> String {
    format!(
        "jj --no-pager --config {JJ_CONFIG} file show --revision @ -- {}",
        encode_path_for_output(path)
    )
}

fn command_diagnostics(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(diagnostics) if !diagnostics.is_empty() => diagnostics.to_owned(),
        Ok(_) => "no diagnostics were emitted".into(),
        Err(_) => "JJ diagnostics could not be rendered".into(),
    }
}

/// Render JJ stderr without lossy replacement or unbounded diagnostics.
fn render_bounded_jj_stderr(bytes: &[u8], truncated: bool) -> String {
    const MARKER: &str = "...[truncated]";
    let raw_len_exceeded = truncated;
    let mut rendered = String::new();
    let mut index = 0;
    while index < bytes.len() {
        match std::str::from_utf8(&bytes[index..]) {
            Ok(text) => {
                rendered.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    if let Some(text) = std::str::from_utf8(&bytes[index..index + valid]).ok() {
                        rendered.push_str(text);
                        index += valid;
                    } else {
                        push_byte_escape(&mut rendered, bytes[index]);
                        index += 1;
                    }
                } else {
                    push_byte_escape(&mut rendered, bytes[index]);
                    index += 1;
                }
            }
        }
    }
    if rendered.ends_with("\r\n") {
        rendered.truncate(rendered.len() - 2);
    } else if rendered.ends_with('\r') || rendered.ends_with('\n') {
        rendered.pop();
    }

    if !raw_len_exceeded && rendered.len() <= JJ_DIAGNOSTIC_LIMIT {
        return rendered;
    }
    let prefix_limit = JJ_DIAGNOSTIC_LIMIT - MARKER.len();
    let mut prefix_len = rendered.len().min(prefix_limit);
    while prefix_len > 0 && !rendered.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    if prefix_len > 0 && rendered.as_bytes()[prefix_len - 1] == b'%' {
        prefix_len -= 1;
    } else if prefix_len >= 2 && rendered.as_bytes()[prefix_len - 2] == b'%' {
        prefix_len -= 2;
    }
    let mut output = rendered;
    output.truncate(prefix_len);
    output.push_str(MARKER);
    debug_assert!(output.len() <= JJ_DIAGNOSTIC_LIMIT);
    debug_assert_eq!(output.matches(MARKER).count(), 1);
    output
}

fn push_byte_escape(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

fn make_manifest(
    source: &SourceContext,
    document: &crate::domain::ParsedDocument,
) -> Result<Manifest, DomainError> {
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
                        Sha256Digest(sha256(&term.logical_bytes)),
                    )
                })
                .collect(),
        })
        .collect();
    Manifest::new(
        MANIFEST_SCHEMA_VERSION,
        SourceIdentity::new(source.canonical_source.clone())
            .with_repository_relative(source.repository_relative.clone()),
        Sha256Digest(sha256(&source.original)),
        document.marker,
        source.original.len(),
        regions,
    )
}

#[derive(Debug)]
struct WorkspaceWriteError {
    path: PathBuf,
    operation: &'static str,
    phase: &'static str,
    source: io::Error,
    /// Set when the operation failed after creating the workspace directory.
    /// The caller must attempt cleanup and report whether it succeeded.
    partial_workspace: Option<PathBuf>,
}

fn workspace_write_error(
    path: &Path,
    operation: &'static str,
    phase: &'static str,
    source: io::Error,
) -> WorkspaceWriteError {
    WorkspaceWriteError {
        path: path.to_owned(),
        operation,
        phase,
        source,
        partial_workspace: None,
    }
}

fn workspace_write_error_after_creation(
    workspace: &Path,
    path: &Path,
    operation: &'static str,
    phase: &'static str,
    source: io::Error,
) -> WorkspaceWriteError {
    let mut error = workspace_write_error(path, operation, phase, source);
    error.partial_workspace = Some(workspace.to_owned());
    error
}

fn workspace_write_error_message(error: &WorkspaceWriteError) -> String {
    format!(
        "operation `{}` failed during phase `{}` for `{}`: {}",
        error.operation,
        error.phase,
        encode_path_for_output(&error.path),
        error.source
    )
}

trait WorkspaceWriter {
    fn create_workspace(
        &mut self,
        output_dir: Option<&Path>,
    ) -> Result<PathBuf, WorkspaceWriteError>;
    fn create_private_directory(&mut self, path: &Path) -> Result<(), WorkspaceWriteError>;
    fn write_exclusive(&mut self, path: &Path, bytes: &[u8]) -> Result<(), WorkspaceWriteError>;
    fn cleanup_workspace(&mut self, workspace: &Path) -> Result<(), WorkspaceWriteError>;
}

struct RealWorkspaceWriter;

impl WorkspaceWriter for RealWorkspaceWriter {
    fn create_workspace(
        &mut self,
        output_dir: Option<&Path>,
    ) -> Result<PathBuf, WorkspaceWriteError> {
        let parent = workspace_parent(output_dir).map_err(|error| match error {
            DomainError::PathUnavailable { path, message } => workspace_write_error(
                &path,
                "create workspace",
                "prepare parent",
                io::Error::new(io::ErrorKind::Other, message),
            ),
            other => workspace_write_error(
                output_dir.unwrap_or_else(|| Path::new(".")),
                "create workspace",
                "prepare parent",
                io::Error::new(io::ErrorKind::Other, other.to_string()),
            ),
        })?;
        for attempt in 0..32u32 {
            let suffix = secure_suffix(attempt).map_err(|error| {
                workspace_write_error(&parent, "create workspace", "secure randomness", error)
            })?;
            let workspace = parent.join(format!("{WORKSPACE_PREFIX}{suffix}"));
            match fs::create_dir(&workspace) {
                Ok(()) => {
                    if let Err(error) = restrict_directory(&workspace) {
                        return Err(workspace_write_error_after_creation(
                            &workspace,
                            &workspace,
                            "create workspace",
                            "restrict permissions",
                            error,
                        ));
                    }
                    return Ok(workspace);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(workspace_write_error(
                        &workspace,
                        "create workspace",
                        "create directory",
                        error,
                    ));
                }
            }
        }
        Err(workspace_write_error(
            &parent,
            "create workspace",
            "choose unique name",
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not find an unused secure workspace name after 32 attempts",
            ),
        ))
    }

    fn create_private_directory(&mut self, path: &Path) -> Result<(), WorkspaceWriteError> {
        fs::create_dir(path).map_err(|error| {
            workspace_write_error(path, "create directory", "create directory", error)
        })?;
        restrict_directory(path).map_err(|error| {
            workspace_write_error(path, "create directory", "restrict permissions", error)
        })
    }

    fn write_exclusive(&mut self, path: &Path, bytes: &[u8]) -> Result<(), WorkspaceWriteError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                workspace_write_error(path, "write artifact", "exclusive-create", error)
            })?;
        restrict_file(&file, path).map_err(|error| {
            workspace_write_error(path, "write artifact", "restrict permissions", error)
        })?;
        file.write_all(bytes).map_err(|error| {
            workspace_write_error(path, "write artifact", "complete-write", error)
        })?;
        file.sync_all()
            .map_err(|error| workspace_write_error(path, "write artifact", "sync", error))
    }

    fn cleanup_workspace(&mut self, workspace: &Path) -> Result<(), WorkspaceWriteError> {
        fs::remove_dir_all(workspace).map_err(|error| {
            workspace_write_error(workspace, "cleanup workspace", "cleanup", error)
        })
    }
}

fn workspace_parent(output_dir: Option<&Path>) -> Result<PathBuf, DomainError> {
    let parent = match output_dir {
        Some(path) => {
            let path = if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()
                    .map_err(|error| DomainError::PathUnavailable {
                        path: path.to_owned(),
                        message: format!("could not resolve output directory: {error}"),
                    })?
                    .join(path)
            };
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                        return Err(DomainError::PathUnavailable {
                            path,
                            message:
                                "output directory must be a real directory, not a symlink or file"
                                    .into(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir_all(&path).map_err(|error| path_error(&path, error))?;
                }
                Err(error) => return Err(path_error(&path, error)),
            }
            path
        }
        None => std::env::temp_dir(),
    };
    let parent = fs::canonicalize(&parent).map_err(|error| path_error(&parent, error))?;
    if !parent.is_dir() {
        return Err(DomainError::PathUnavailable {
            path: parent,
            message: "workspace parent is not a directory".into(),
        });
    }
    Ok(parent)
}

fn secure_suffix(_attempt: u32) -> io::Result<String> {
    let mut bytes = [0u8; 16];
    #[cfg(unix)]
    {
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    #[cfg(windows)]
    {
        use std::os::raw::{c_ulong, c_void};
        unsafe extern "system" {
            fn BCryptGenRandom(
                algorithm: *mut c_void,
                buffer: *mut u8,
                length: c_ulong,
                flags: c_ulong,
            ) -> i32;
        }
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: c_ulong = 0x00000002;
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as c_ulong,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("BCryptGenRandom failed with status 0x{status:08x}"),
            ));
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure workspace randomness is unavailable on this platform",
        ));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_workspace<W: WorkspaceWriter>(
    writer: &mut W,
    workspace: &Path,
    source: &[u8],
    document: &crate::domain::ParsedDocument,
    resolved: &[u8],
    manifest: &Manifest,
) -> Result<(), WorkspaceWriteError> {
    manifest.validate().map_err(|error| {
        workspace_write_error(
            &workspace.join("manifest.json"),
            "validate manifest",
            "validate manifest",
            io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        )
    })?;
    writer.write_exclusive(&workspace.join("source"), source)?;
    let regions = workspace.join("regions");
    writer.create_private_directory(&regions)?;
    for (region_index, region) in document.regions.iter().enumerate() {
        let region_dir = regions.join(format!("region-{region_index:03}"));
        writer.create_private_directory(&region_dir)?;
        for term in &region.terms {
            let artifact = region_dir.join(format!("term-{:03}.term", term.ordinal));
            writer.write_exclusive(&artifact, &term.logical_bytes)?;
        }
    }
    writer.write_exclusive(&workspace.join("resolved"), resolved)?;
    writer.write_exclusive(&workspace.join("manifest.json"), &manifest_json(manifest))?;
    Ok(())
}

fn workspace_failure_error(error: WorkspaceWriteError) -> DomainError {
    DomainError::PathUnavailable {
        path: error.path.clone(),
        message: workspace_write_error_message(&error),
    }
}

fn cleanup_failed_workspace<W: WorkspaceWriter>(
    writer: &mut W,
    workspace: PathBuf,
    error: WorkspaceWriteError,
) -> DomainError {
    let error_path = error.path.clone();
    let error_message = workspace_write_error_message(&error);
    match writer.cleanup_workspace(&workspace) {
        Ok(()) => DomainError::PathUnavailable {
            path: error_path,
            message: format!(
                "could not complete workspace artifacts: {error_message}; cleanup completed"
            ),
        },
        Err(cleanup_error) => DomainError::PathUnavailable {
            path: error_path,
            message: format!(
                "could not complete workspace artifacts: {error_message}; cleanup failed during phase `{}` for `{}`: {}; inspect the retained partial workspace",
                cleanup_error.phase,
                encode_path_for_output(&cleanup_error.path),
                cleanup_error.source
            ),
        },
    }
}

fn path_error(path: &Path, error: io::Error) -> DomainError {
    DomainError::PathUnavailable {
        path: path.to_owned(),
        message: error.to_string(),
    }
}

pub(crate) fn manifest_json(manifest: &Manifest) -> Vec<u8> {
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!(
        "  \"schema_version\": {},\n",
        manifest.schema_version
    ));
    json.push_str("  \"source\": {\n");
    json.push_str("    \"canonical_path\": ");
    push_json_string(&mut json, &manifest.source.canonical_path.to_string_lossy());
    json.push_str(",\n    \"canonical_path_bytes_hex\": ");
    push_json_string(
        &mut json,
        &os_bytes_hex(manifest.source.canonical_path.as_os_str()),
    );
    json.push_str(",\n    \"repository_relative\": ");
    match &manifest.source.repository_relative {
        Some(path) => push_json_string(&mut json, &path.to_string_lossy()),
        None => json.push_str("null"),
    }
    json.push_str(",\n    \"repository_relative_bytes_hex\": ");
    match &manifest.source.repository_relative {
        Some(path) => push_json_string(&mut json, &os_bytes_hex(path.as_os_str())),
        None => json.push_str("null"),
    }
    json.push_str("\n  },\n");
    json.push_str("  \"source_sha256\": ");
    push_json_string(&mut json, &hex_digest(manifest.source_digest));
    json.push_str(&format!(
        ",\n  \"source_length\": {},\n  \"region_count\": {},\n",
        manifest.source_length,
        manifest.regions.len()
    ));
    json.push_str("  \"marker\": {\n");
    json.push_str("    \"style\": \"Snapshot\",\n");
    json.push_str(&format!(
        "    \"outer_marker_width\": {},\n    \"section_marker_width\": {}\n  }},\n",
        manifest.marker.outer_marker_width, manifest.marker.section_marker_width
    ));
    json.push_str("  \"regions\": [\n");
    for (region_position, region) in manifest.regions.iter().enumerate() {
        if region_position > 0 {
            json.push_str(",\n");
        }
        json.push_str("    {\n");
        json.push_str(&format!(
            "      \"region_index\": {},\n      \"source_range\": {{\"start\": {}, \"end\": {}}},\n      \"seed\": ",
            region.region_index,
            region.source_range.start,
            region.source_range.end,
        ));
        push_json_string(&mut json, &String::from_utf8_lossy(&region.seed));
        json.push_str(&format!(
            ",\n      \"term_count\": {},\n      \"terms\": [\n",
            region.terms.len()
        ));
        for (term_position, term) in region.terms.iter().enumerate() {
            if term_position > 0 {
                json.push_str(",\n");
            }
            json.push_str("        {\n");
            json.push_str(&format!(
                "          \"ordinal\": {},\n          \"kind\": ",
                term.ordinal
            ));
            push_json_string(&mut json, &term.kind.to_string());
            json.push_str(",\n          \"label\": ");
            push_json_string(&mut json, &term.label);
            json.push_str(&format!(
                ",\n          \"logical_length\": {},\n          \"sha256\": ",
                term.logical_length
            ));
            push_json_string(&mut json, &hex_digest(term.digest));
            json.push_str(&format!(
                ",\n          \"logical_final_newline\": {},\n          \"synthetic_separator_eol_removed\": {},\n          \"artifact_path\": ",
                term.logical_final_newline, term.synthetic_separator_eol_removed
            ));
            push_json_string(&mut json, &term.artifact_path.to_string_lossy());
            json.push_str("\n        }");
        }
        json.push_str("\n      ]\n    }");
    }
    json.push_str("\n  ]\n}\n");
    json.into_bytes()
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn hex_digest(digest: Sha256Digest) -> String {
    digest.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn os_bytes_hex(value: &OsStr) -> String {
    os_bytes(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(file: &File, _path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_file: &File, _path: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn sha256(input: &[u8]) -> [u8; 32] {
    Sha256::digest(input).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ByteRange, SnapshotStyle, TermKind};
    use serde_json::Value;

    #[test]
    fn sha256_matches_standard_vectors() {
        let inputs = [55, 56, 63, 64, 65].map(|length| vec![b'a'; length]);
        let vectors = [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                inputs[0].as_slice(),
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                inputs[1].as_slice(),
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                inputs[2].as_slice(),
                "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34",
            ),
            (
                inputs[3].as_slice(),
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
            (
                inputs[4].as_slice(),
                "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(hex_digest(Sha256Digest(sha256(input))), expected);
        }
    }

    #[test]
    fn manifest_json_contains_safe_layout_and_metadata() {
        let seed = b"JCW-UNRESOLVED-CONFLICT-REGION-000: replace this line. Terms: regions/region-000/term-000.term\n";
        let manifest = Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            SourceIdentity::new("/repo/file").with_repository_relative("file"),
            Sha256Digest(sha256(b"source")),
            crate::domain::SnapshotMarker::new(SnapshotStyle::Snapshot, 7, 7).unwrap(),
            6,
            vec![ManifestRegion {
                region_index: 0,
                source_range: ByteRange::new(1, 2).unwrap(),
                seed: seed.to_vec().into_boxed_slice(),
                terms: vec![ManifestTerm {
                    ordinal: 0,
                    kind: TermKind::Side,
                    label: "side \"one\"".into(),
                    logical_length: 0,
                    digest: Sha256Digest::ZERO,
                    logical_final_newline: false,
                    synthetic_separator_eol_removed: true,
                    artifact_path: ManifestTerm::generated_artifact_path(0, 0),
                }],
            }],
        )
        .unwrap();
        let json = String::from_utf8(manifest_json(&manifest)).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["region_count"], Value::from(1));
        assert_eq!(
            parsed["regions"][0]["terms"][0]["label"],
            Value::from("side \"one\"")
        );
        assert_eq!(
            parsed["regions"][0]["seed"],
            Value::from(String::from_utf8(seed.to_vec()).unwrap())
        );
        assert!(json.contains("\"region_count\": 1"));
        assert!(json.contains("regions/region-000/term-000.term"));
        assert!(json.contains("side \\\"one\\\""));
        assert!(json.contains("\"logical_length\": 0"));
    }

    #[test]
    fn manifest_json_round_trips_region_seeds_through_decode_manifest() {
        let source =
            b"prefix\n<<<<<<< open\n+++++++ side\nx\n------- base\ny\n>>>>>>> close\nsuffix\n";
        let document = crate::core::parse_snapshot(source).unwrap();
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
                            Sha256Digest(sha256(&term.logical_bytes)),
                        )
                    })
                    .collect(),
            })
            .collect();
        let manifest = Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            SourceIdentity::new("/repo/file").with_repository_relative("file"),
            Sha256Digest(sha256(source)),
            document.marker,
            source.len(),
            regions,
        )
        .unwrap();
        let decoded =
            crate::apply::decode_manifest(&manifest_json(&manifest), std::path::Path::new("m"))
                .unwrap();
        assert_eq!(decoded.regions.len(), manifest.regions.len());
        for (expected, actual) in manifest.regions.iter().zip(&decoded.regions) {
            assert_eq!(expected.seed, actual.seed, "region seed bytes");
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InjectedFailure {
        CreateWorkspace,
        CreateDirectory,
        ExclusiveCreate,
        CompleteWrite,
        Sync,
        Cleanup,
    }

    struct FaultWriter {
        root: PathBuf,
        failure: InjectedFailure,
        cleanup_failure: bool,
        failed: bool,
    }

    impl FaultWriter {
        fn new(root: PathBuf, failure: InjectedFailure, cleanup_failure: bool) -> Self {
            Self {
                root,
                failure,
                cleanup_failure,
                failed: false,
            }
        }

        fn should_fail(&mut self, failure: InjectedFailure) -> bool {
            if !self.failed && self.failure == failure {
                self.failed = true;
                true
            } else {
                false
            }
        }

        fn injected_error(
            path: &Path,
            operation: &'static str,
            phase: &'static str,
        ) -> WorkspaceWriteError {
            workspace_write_error(
                path,
                operation,
                phase,
                io::Error::new(io::ErrorKind::Other, "injected failure"),
            )
        }
    }

    impl WorkspaceWriter for FaultWriter {
        fn create_workspace(
            &mut self,
            _output_dir: Option<&Path>,
        ) -> Result<PathBuf, WorkspaceWriteError> {
            let workspace = self.root.join("jcw-injected");
            if self.should_fail(InjectedFailure::CreateWorkspace) {
                return Err(Self::injected_error(
                    &workspace,
                    "create workspace",
                    "create directory",
                ));
            }
            fs::create_dir(&workspace).map_err(|error| {
                workspace_write_error(&workspace, "create workspace", "create directory", error)
            })?;
            restrict_directory(&workspace).map_err(|error| {
                workspace_write_error_after_creation(
                    &workspace,
                    &workspace,
                    "create workspace",
                    "restrict permissions",
                    error,
                )
            })?;
            Ok(workspace)
        }

        fn create_private_directory(&mut self, path: &Path) -> Result<(), WorkspaceWriteError> {
            if self.should_fail(InjectedFailure::CreateDirectory) {
                return Err(Self::injected_error(
                    path,
                    "create directory",
                    "create directory",
                ));
            }
            fs::create_dir(path).map_err(|error| {
                workspace_write_error(path, "create directory", "create directory", error)
            })?;
            restrict_directory(path).map_err(|error| {
                workspace_write_error(path, "create directory", "restrict permissions", error)
            })
        }

        fn write_exclusive(
            &mut self,
            path: &Path,
            bytes: &[u8],
        ) -> Result<(), WorkspaceWriteError> {
            if self.should_fail(InjectedFailure::ExclusiveCreate) {
                return Err(Self::injected_error(
                    path,
                    "write artifact",
                    "exclusive-create",
                ));
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| {
                    workspace_write_error(path, "write artifact", "exclusive-create", error)
                })?;
            restrict_file(&file, path).map_err(|error| {
                workspace_write_error(path, "write artifact", "restrict permissions", error)
            })?;
            if self.should_fail(InjectedFailure::CompleteWrite) {
                return Err(Self::injected_error(
                    path,
                    "write artifact",
                    "complete-write",
                ));
            }
            file.write_all(bytes).map_err(|error| {
                workspace_write_error(path, "write artifact", "complete-write", error)
            })?;
            if self.should_fail(InjectedFailure::Sync) {
                return Err(Self::injected_error(path, "write artifact", "sync"));
            }
            file.sync_all()
                .map_err(|error| workspace_write_error(path, "write artifact", "sync", error))
        }

        fn cleanup_workspace(&mut self, workspace: &Path) -> Result<(), WorkspaceWriteError> {
            if self.cleanup_failure || self.should_fail(InjectedFailure::Cleanup) {
                return Err(Self::injected_error(
                    workspace,
                    "cleanup workspace",
                    "cleanup",
                ));
            }
            fs::remove_dir_all(workspace).map_err(|error| {
                workspace_write_error(workspace, "cleanup workspace", "cleanup", error)
            })
        }
    }

    fn minimal_snapshot(_source: &SourceContext) -> Result<Vec<u8>, DomainError> {
        Ok(b"<<<<<<< opening\n+++++++ side\nside\n------- base\nbase\n>>>>>>> closing\n".to_vec())
    }

    fn writer_test_fixture(name: &str) -> (tempfile::TempDir, PrepareOptions) {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        fs::create_dir(&repository).unwrap();
        fs::create_dir(repository.join(".jj")).unwrap();
        let source = repository.join("source");
        fs::write(&source, vec![b'x'; 1024]).unwrap();
        let output = temporary.path().join(name);
        fs::create_dir(&output).unwrap();
        (
            temporary,
            PrepareOptions {
                file: source,
                output_dir: Some(output),
            },
        )
    }

    #[test]
    fn workspace_writer_reports_each_operation_and_cleanup_status() {
        let failures = [
            (
                InjectedFailure::CreateWorkspace,
                "create workspace",
                "create directory",
            ),
            (
                InjectedFailure::CreateDirectory,
                "create directory",
                "create directory",
            ),
            (
                InjectedFailure::ExclusiveCreate,
                "write artifact",
                "exclusive-create",
            ),
            (
                InjectedFailure::CompleteWrite,
                "write artifact",
                "complete-write",
            ),
            (InjectedFailure::Sync, "write artifact", "sync"),
        ];
        for (failure, operation, phase) in failures {
            let (temporary, options) = writer_test_fixture("output");
            let before = fs::read(&options.file).unwrap();
            let mut writer = FaultWriter::new(options.output_dir.clone().unwrap(), failure, false);
            let error = run_with_writer(&options, &mut writer, minimal_snapshot).unwrap_err();
            let rendered = error.to_string();
            assert!(rendered.contains(operation), "{rendered}");
            assert!(rendered.contains(phase), "{rendered}");
            if failure != InjectedFailure::CreateWorkspace {
                assert!(rendered.contains("cleanup completed"), "{rendered}");
            }
            assert_eq!(fs::read(&options.file).unwrap(), before);
            drop(temporary);
        }

        let (temporary, options) = writer_test_fixture("retained-output");
        let mut writer = FaultWriter::new(
            options.output_dir.clone().unwrap(),
            InjectedFailure::CompleteWrite,
            true,
        );
        let error = run_with_writer(&options, &mut writer, minimal_snapshot).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("complete-write"), "{rendered}");
        assert!(rendered.contains("cleanup failed"), "{rendered}");
        assert!(
            rendered.contains("retained partial workspace"),
            "{rendered}"
        );
        assert!(
            options
                .output_dir
                .as_ref()
                .unwrap()
                .join("jcw-injected")
                .exists()
        );
        drop(temporary);
    }

    #[test]
    fn file_identity_capture_uses_open_handle_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("source");
        fs::write(&path, b"bytes").unwrap();
        let file = File::open(&path).unwrap();
        let metadata = file.metadata().unwrap();

        // `capture` obtains identity fields from the already-open handle.  The
        // test asserts the observable contract without pretending that a
        // pre-epoch `Metadata` value can be fabricated portably.
        let identity = FileIdentity::capture(&file, &metadata).unwrap();
        assert_eq!(identity.file_type, "regular");
        assert_eq!(identity.len, 5);
        assert!(identity.modified_seconds >= 0);
    }

    #[test]
    fn render_bounded_jj_stderr_is_byte_bounded_and_escapes_invalid_bytes() {
        let exact = vec![b'a'; JJ_DIAGNOSTIC_LIMIT];
        let rendered = render_bounded_jj_stderr(&exact, false);
        assert_eq!(rendered.len(), JJ_DIAGNOSTIC_LIMIT);
        assert!(!rendered.contains("...[truncated]"));

        let oversized = vec![b'b'; JJ_DIAGNOSTIC_LIMIT + 1];
        let rendered = render_bounded_jj_stderr(&oversized, true);
        assert_eq!(rendered.len(), JJ_DIAGNOSTIC_LIMIT);
        assert_eq!(rendered.matches("...[truncated]").count(), 1);

        let invalid = render_bounded_jj_stderr(b"bad\xff\xfe\r\n", false);
        assert_eq!(invalid, "bad%FF%FE");
    }

    #[test]
    fn render_bounded_jj_stderr_handles_near_limit_all_invalid_bytes() {
        let input = vec![0xff; JJ_DIAGNOSTIC_LIMIT / 3 + 1];
        let rendered = render_bounded_jj_stderr(&input, false);
        // The marker leaves 65,522 bytes for the prefix.  Since each invalid
        // byte expands to a complete three-byte escape, two bytes necessarily
        // remain unavailable; never pad with a partial escape or synthetic data.
        assert_eq!(rendered.len(), JJ_DIAGNOSTIC_LIMIT - 2);
        assert_eq!(rendered.matches("...[truncated]").count(), 1);
        assert!(rendered.starts_with("%FF%FF"));
        assert_eq!(
            rendered[..rendered.len() - "...[truncated]".len()].len() % 3,
            0
        );
    }

    #[test]
    fn render_bounded_jj_stderr_marks_rendered_overflow_below_raw_limit() {
        let raw_len = JJ_DIAGNOSTIC_LIMIT / 3 + 1;
        let input = vec![0xfe; raw_len];
        let rendered = render_bounded_jj_stderr(&input, false);
        assert!(raw_len < JJ_DIAGNOSTIC_LIMIT);
        assert_eq!(rendered.len(), JJ_DIAGNOSTIC_LIMIT - 2);
        assert_eq!(rendered.matches("...[truncated]").count(), 1);
        let prefix = &rendered[..rendered.len() - "...[truncated]".len()];
        assert_eq!(prefix.len() % 3, 0);
        assert!(!prefix.ends_with('%'));
    }

    #[test]
    fn line_context_reports_line_number_and_content_for_offsets() {
        let snapshot = b"<script lang=\"ts\">\nconst x = 1;\nline with \r\nend";
        // First line start.
        assert_eq!(
            line_context(snapshot, 0),
            Some((1, "<script lang=\"ts\">".into()))
        );
        // Inside the first line.
        assert_eq!(
            line_context(snapshot, 3),
            Some((1, "<script lang=\"ts\">".into()))
        );
        // Second line.
        assert_eq!(line_context(snapshot, 20), Some((2, "const x = 1;".into())));
        // CRLF line excludes the carriage return from the content.
        assert_eq!(line_context(snapshot, 38), Some((3, "line with ".into())));
        // Offset at EOF maps to the final unterminated line.
        assert_eq!(
            line_context(snapshot, snapshot.len()),
            Some((4, "end".into()))
        );
        // Offsets past EOF are not a line.
        assert_eq!(line_context(snapshot, snapshot.len() + 1), None);
        // Long lines are truncated without splitting a character.
        let long = vec![b'<'; 200];
        let (line, content) = line_context(&long, 0).unwrap();
        assert_eq!(line, 1);
        assert_eq!(content.chars().count(), 80);
    }

    #[test]
    fn describe_parse_failure_distinguishes_no_conflict_from_malformed() {
        let relative = Path::new("client/src/components/Transcript.svelte");
        let snapshot = b"<script lang=\"ts\">\nconst x = 1;\n";

        let no_conflict = DomainError::NoConflictFound {
            path: relative.to_path_buf(),
        };
        let rendered = no_conflict.to_string();
        assert!(
            rendered.contains("no conflict found in `client/src/components/Transcript.svelte`")
        );
        assert!(rendered.contains("nothing to prepare"));
        assert!(!rendered.contains("could not be parsed"));
        assert!(!rendered.contains("invalid input:"));

        let malformed = describe_parse_failure(
            relative,
            snapshot,
            DomainError::InvalidInput {
                message: "opening marker run has width 9; expected document width 7".into(),
                region_index: Some(1),
                byte_offset: Some(20),
                range: None,
            },
        );
        let rendered = malformed.to_string();
        assert!(rendered.contains("could not be parsed"));
        assert!(rendered.contains("at line 2: `const x = 1;`"));
        assert!(rendered.contains("(region 1)"));
        assert!(!rendered.contains("byte offset 20"));
        assert_eq!(rendered.matches("invalid input:").count(), 1);

        let unsupported = describe_parse_failure(
            relative,
            snapshot,
            DomainError::UnsupportedStyle {
                style: "Git".into(),
            },
        );
        assert!(
            unsupported
                .to_string()
                .contains("unsupported snapshot style `Git`")
        );
    }
}
