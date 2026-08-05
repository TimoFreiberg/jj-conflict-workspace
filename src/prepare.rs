use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;
#[cfg(not(unix))]
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::PrepareOptions;
use crate::core::{materialize_scaffold, parse_snapshot};
use crate::domain::{
    MANIFEST_SCHEMA_VERSION, Manifest, ManifestRegion, ManifestTerm, Sha256Digest, SourceIdentity,
};
use crate::error::DomainError;

const JJ_PROGRAM: &str = "jj";
const JJ_CONFIG: &str = "ui.conflict-marker-style=snapshot";
const WORKSPACE_PREFIX: &str = "jcw-";

#[derive(Debug)]
struct SourceContext {
    canonical_source: PathBuf,
    repository_root: PathBuf,
    repository_relative: PathBuf,
    original: Vec<u8>,
}

/// Prepare a source file into a private, persistent workspace.
///
/// The child process invocation is intentionally kept here, at the imperative
/// boundary. The parser and scaffold materializer remain pure functions.
pub fn run(options: &PrepareOptions) -> Result<PathBuf, DomainError> {
    let source = validate_source(&options.file)?;
    let snapshot = invoke_jj(&source)?;
    let document = parse_snapshot(&snapshot).map_err(|error| {
        DomainError::invalid(format!(
            "JJ snapshot output for `{}` could not be parsed: {error}",
            source.repository_relative.display()
        ))
    })?;
    let resolved = materialize_scaffold(&document).map_err(|error| {
        DomainError::invalid(format!(
            "JJ snapshot output could not be materialized: {error}"
        ))
    })?;

    let manifest = make_manifest(&source, &document)?;
    let workspace = create_workspace(options.output_dir.as_deref())?;
    match write_workspace(
        &workspace,
        &source.original,
        &document,
        &resolved,
        &manifest,
    ) {
        Ok(()) => Ok(workspace),
        Err(error) => Err(cleanup_failed_workspace(workspace, error)),
    }
}

fn validate_source(path: &Path) -> Result<SourceContext, DomainError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if !metadata.file_type().is_file() {
        return Err(DomainError::PathUnavailable {
            path: path.to_owned(),
            message: "the source must be a regular file, not a directory or symlink".into(),
        });
    }

    let current_dir = std::env::current_dir().map_err(|error| DomainError::PathUnavailable {
        path: path.to_owned(),
        message: format!("could not determine the current directory: {error}"),
    })?;
    let current_dir =
        fs::canonicalize(&current_dir).map_err(|error| path_error(&current_dir, error))?;
    let repository_root =
        find_repository_root(&current_dir).ok_or_else(|| DomainError::PathUnavailable {
            path: path.to_owned(),
            message: "the current directory is not inside a JJ repository".into(),
        })?;
    let canonical_source = fs::canonicalize(path).map_err(|error| path_error(path, error))?;
    let repository_relative = canonical_source
        .strip_prefix(&repository_root)
        .map_err(|_| DomainError::PathUnavailable {
            path: path.to_owned(),
            message: format!(
                "the canonical source `{}` is outside repository `{}`",
                canonical_source.display(),
                repository_root.display()
            ),
        })?
        .to_owned();

    validate_relative_path(&repository_relative, path)?;
    let original = fs::read(&canonical_source).map_err(|error| path_error(path, error))?;

    Ok(SourceContext {
        canonical_source,
        repository_root,
        repository_relative,
        original,
    })
}

fn find_repository_root(start: &Path) -> Option<PathBuf> {
    let mut candidate = Some(start);
    while let Some(path) = candidate {
        let jj_dir = path.join(".jj");
        if fs::symlink_metadata(&jj_dir)
            .map(|metadata| metadata.file_type().is_dir())
            .unwrap_or(false)
        {
            return Some(path.to_owned());
        }
        candidate = path.parent();
    }
    None
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
        .current_dir(&source.repository_root);

    let description = format_command(&source.repository_relative);
    let output = command
        .output()
        .map_err(|error| DomainError::ExternalCommand {
            command: description.clone(),
            status: None,
            stderr: format!("could not start JJ: {error}"),
        })?;
    if !output.status.success() {
        return Err(DomainError::ExternalCommand {
            command: description,
            status: output.status.code(),
            stderr: command_diagnostics(&output.stderr),
        });
    }
    Ok(output.stdout)
}

fn format_command(path: &Path) -> String {
    format!(
        "jj --no-pager --config {JJ_CONFIG} file show --revision @ -- {}",
        display_os(path.as_os_str())
    )
}

fn command_diagnostics(bytes: &[u8]) -> String {
    let diagnostics = String::from_utf8_lossy(bytes).trim().to_owned();
    if diagnostics.is_empty() {
        "no diagnostics were emitted".into()
    } else {
        diagnostics
    }
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

fn create_workspace(output_dir: Option<&Path>) -> Result<PathBuf, DomainError> {
    let parent = workspace_parent(output_dir)?;
    for attempt in 0..32u32 {
        let suffix = secure_suffix(attempt).map_err(|error| DomainError::PathUnavailable {
            path: parent.clone(),
            message: format!("could not obtain secure workspace randomness: {error}"),
        })?;
        let workspace = parent.join(format!("{WORKSPACE_PREFIX}{suffix}"));
        match fs::create_dir(&workspace) {
            Ok(()) => {
                if let Err(error) = restrict_directory(&workspace) {
                    let _ = fs::remove_dir(&workspace);
                    return Err(DomainError::PathUnavailable {
                        path: workspace,
                        message: format!("could not restrict workspace permissions: {error}"),
                    });
                }
                return Ok(workspace);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(DomainError::PathUnavailable {
                    path: workspace,
                    message: format!("could not create private workspace: {error}"),
                });
            }
        }
    }
    Err(DomainError::PathUnavailable {
        path: parent,
        message: "could not find an unused secure workspace name after 32 attempts".into(),
    })
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
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(DomainError::PathUnavailable {
                        path,
                        message: "output directory must be a real directory, not a symlink or file"
                            .into(),
                    });
                }
            } else {
                fs::create_dir_all(&path).map_err(|error| path_error(&path, error))?;
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
    #[cfg(not(unix))]
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id() as u128;
        let value = now ^ (pid << 64) ^ u128::from(attempt);
        bytes.copy_from_slice(&value.to_le_bytes());
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_workspace(
    workspace: &Path,
    source: &[u8],
    document: &crate::domain::ParsedDocument,
    resolved: &[u8],
    manifest: &Manifest,
) -> io::Result<()> {
    manifest
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    write_exclusive(&workspace.join("source"), source)?;
    let regions = workspace.join("regions");
    create_private_directory(&regions)?;
    for (region_index, region) in document.regions.iter().enumerate() {
        let region_dir = regions.join(format!("region-{region_index:03}"));
        create_private_directory(&region_dir)?;
        for term in &region.terms {
            let artifact = region_dir.join(format!("term-{:03}.term", term.ordinal));
            write_exclusive(&artifact, &term.logical_bytes)?;
        }
    }
    write_exclusive(&workspace.join("resolved"), resolved)?;
    write_exclusive(&workspace.join("manifest.json"), &manifest_json(manifest))?;
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    restrict_directory(path)
}

fn write_exclusive(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    restrict_file(&file, path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn cleanup_failed_workspace(workspace: PathBuf, error: io::Error) -> DomainError {
    match fs::remove_dir_all(&workspace) {
        Ok(()) => DomainError::PathUnavailable {
            path: workspace,
            message: format!("could not complete workspace artifacts: {error}; cleanup completed"),
        },
        Err(cleanup_error) => DomainError::PathUnavailable {
            path: workspace,
            message: format!(
                "could not complete workspace artifacts: {error}; cleanup failed: {cleanup_error}; inspect the retained partial workspace"
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

fn manifest_json(manifest: &Manifest) -> Vec<u8> {
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
            "      \"region_index\": {},\n      \"source_range\": {{\"start\": {}, \"end\": {}}},\n      \"term_count\": {},\n      \"terms\": [\n",
            region.region_index,
            region.source_range.start,
            region.source_range.end,
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

fn display_os(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
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
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let padded_len = (input.len() + 9).div_ceil(64) * 64;
    let mut padded = vec![0u8; padded_len];
    padded[..input.len()].copy_from_slice(input);
    padded[input.len()] = 0x80;
    padded[padded_len - 8..].copy_from_slice(&bit_len.to_be_bytes());

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    for chunk in padded.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (index, word) in schedule[..16].iter_mut().enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }
        let mut working = state;
        for index in 0..64 {
            let choice = (working[4] & working[5]) ^ ((!working[4]) & working[6]);
            let majority =
                (working[0] & working[1]) ^ (working[0] & working[2]) ^ (working[1] & working[2]);
            let s1 = working[4].rotate_right(6)
                ^ working[4].rotate_right(11)
                ^ working[4].rotate_right(25);
            let s0 = working[0].rotate_right(2)
                ^ working[0].rotate_right(13)
                ^ working[0].rotate_right(22);
            let temp1 = working[7]
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let temp2 = s0.wrapping_add(majority);
            working[7] = working[6];
            working[6] = working[5];
            working[5] = working[4];
            working[4] = working[3].wrapping_add(temp1);
            working[3] = working[2];
            working[2] = working[1];
            working[1] = working[0];
            working[0] = temp1.wrapping_add(temp2);
        }
        for index in 0..8 {
            state[index] = state[index].wrapping_add(working[index]);
        }
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ByteRange, SnapshotStyle, TermKind};

    #[test]
    fn sha256_matches_standard_vectors() {
        assert_eq!(
            hex_digest(Sha256Digest(sha256(b""))),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_digest(Sha256Digest(sha256(b"abc"))),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifest_json_contains_safe_layout_and_metadata() {
        let manifest = Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            SourceIdentity::new("/repo/file").with_repository_relative("file"),
            Sha256Digest(sha256(b"source")),
            crate::domain::SnapshotMarker::new(SnapshotStyle::Snapshot, 7, 7).unwrap(),
            6,
            vec![ManifestRegion {
                region_index: 0,
                source_range: ByteRange::new(1, 2).unwrap(),
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
        assert!(json.contains("\"region_count\": 1"));
        assert!(json.contains("regions/region-000/term-000.term"));
        assert!(json.contains("side \\\"one\\\""));
        assert!(json.contains("\"logical_length\": 0"));
    }
}
