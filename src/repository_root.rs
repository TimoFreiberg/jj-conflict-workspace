//! Shared canonical repository-root and repository-relative path resolution.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepositorySource {
    pub(crate) canonical_source: PathBuf,
    pub(crate) repository_root: PathBuf,
    pub(crate) repository_relative: PathBuf,
}

/// Resolve a source and the nearest ancestor containing a real `.jj` directory.
///
/// The source is canonicalized before walking. Each ancestor is considered from
/// the source directory upward, so nested repositories select the nearest root.
/// A symlink named `.jj` is deliberately not accepted as a repository marker.
pub(crate) fn resolve_repository_source(path: &Path) -> io::Result<RepositorySource> {
    let canonical_source = fs::canonicalize(path)?;
    let source_metadata = fs::symlink_metadata(&canonical_source)?;
    if !source_metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }

    let source_directory = canonical_source.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "canonical source has no parent directory",
        )
    })?;

    let mut candidate = Some(source_directory);
    let repository_root = loop {
        let candidate_path = candidate.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no ancestor contains a real .jj directory",
            )
        })?;
        let marker = candidate_path.join(".jj");
        match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.file_type().is_dir() => break candidate_path.to_owned(),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        candidate = candidate_path.parent();
    };

    let repository_relative = canonical_source
        .strip_prefix(&repository_root)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "canonical source is outside the resolved repository root",
            )
        })?
        .to_owned();
    if repository_relative.as_os_str().is_empty() || repository_relative.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source does not have a repository-relative path",
        ));
    }

    Ok(RepositorySource {
        canonical_source,
        repository_root,
        repository_relative,
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_repository_source;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn chooses_nearest_real_repository_marker() {
        let temporary = tempdir().unwrap();
        let outer = temporary.path();
        fs::create_dir(outer.join(".jj")).unwrap();
        let nested = outer.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::create_dir(nested.join(".jj")).unwrap();
        let source = nested.join("file");
        fs::write(&source, b"source").unwrap();

        let resolved = resolve_repository_source(&source).unwrap();
        assert_eq!(resolved.repository_root, fs::canonicalize(&nested).unwrap());
        assert_eq!(
            resolved.repository_relative,
            std::path::PathBuf::from("file")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_repository_marker() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let marker_target = temporary.path().join("marker-target");
        fs::create_dir(&marker_target).unwrap();
        symlink(&marker_target, temporary.path().join(".jj")).unwrap();
        let source = temporary.path().join("file");
        fs::write(&source, b"source").unwrap();

        let error = resolve_repository_source(&source).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
