use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    let binary = std::env::var_os("CARGO_BIN_EXE_jj_conflict_untangler")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_owned))
                .map(|target_debug| target_debug.join("jj-conflict-untangler"))
        })
        .expect("Cargo built the jj-conflict-untangler binary");
    Command::new(binary)
        .args(args)
        .output()
        .expect("run placeholder binary")
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("snapshot path is below root")
                .to_owned();
            if path.is_dir() {
                visit(root, &path, snapshot);
            } else if let Ok(bytes) = fs::read(&path) {
                snapshot.insert(relative, bytes);
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

#[test]
fn valid_prepare_and_apply_commands_are_safe_placeholders() {
    let corpus = Path::new("docs/test-corpus");
    let before = snapshot_tree(corpus);
    let prepare_artifacts = [
        Path::new("unused-output"),
        Path::new("definitely-not-present"),
    ];
    let prepare_existence: Vec<_> = prepare_artifacts
        .iter()
        .map(|path| (path.to_owned(), path.exists()))
        .collect();
    let prepare = run(&[
        "prepare",
        "--file",
        "definitely-not-present",
        "--output-dir",
        "unused-output",
    ]);
    assert!(!prepare.status.success());
    assert_eq!(prepare.stdout, b"");
    assert_eq!(
        String::from_utf8_lossy(&prepare.stderr),
        "prepare is not implemented yet\n"
    );
    assert_eq!(snapshot_tree(corpus), before);
    for (path, existed) in &prepare_existence {
        assert_eq!(
            path.exists(),
            *existed,
            "placeholder changed {}",
            path.display()
        );
    }

    let apply_artifacts = [
        Path::new("definitely-not-resolved"),
        Path::new("definitely-not-manifest"),
    ];
    let apply_existence: Vec<_> = apply_artifacts
        .iter()
        .map(|path| (path.to_owned(), path.exists()))
        .collect();
    let apply = run(&[
        "apply",
        "--resolved-file",
        "definitely-not-resolved",
        "--manifest",
        "definitely-not-manifest",
        "--write",
    ]);
    assert!(!apply.status.success());
    assert_eq!(apply.stdout, b"");
    assert_eq!(
        String::from_utf8_lossy(&apply.stderr),
        "apply is not implemented yet\n"
    );
    assert_eq!(snapshot_tree(corpus), before);
    for (path, existed) in &apply_existence {
        assert_eq!(
            path.exists(),
            *existed,
            "placeholder changed {}",
            path.display()
        );
    }
}

#[test]
fn help_succeeds_and_no_command_is_a_stable_error() {
    let corpus = Path::new("docs/test-corpus");
    let before = snapshot_tree(corpus);
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert_eq!(
        String::from_utf8_lossy(&help.stdout),
        concat!(
            "Usage:\n",
            "  jj-conflict-untangler prepare --file FILE [--output-dir DIR]\n",
            "  jj-conflict-untangler apply --resolved-file FILE [--manifest FILE] [--write]\n",
            "  jj-conflict-untangler --help\n",
        )
    );
    assert_eq!(help.stderr, b"");

    let missing = run(&[]);
    assert!(!missing.status.success());
    assert_eq!(missing.stdout, b"");
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .starts_with("error: missing command; choose `prepare` or `apply`\n")
    );
    assert_eq!(snapshot_tree(corpus), before);
}
