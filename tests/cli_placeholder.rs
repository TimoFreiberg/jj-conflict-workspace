use std::collections::BTreeMap;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn binary() -> PathBuf {
    env::var_os("CARGO_BIN_EXE_jcw")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_owned))
                .map(|target_debug| target_debug.join("jcw"))
        })
        .expect("Cargo built the jcw binary")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(binary()).args(args).output().unwrap()
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = env::temp_dir().join(format!("jcw-test-{name}-{suffix}"));
    fs::create_dir(&path).unwrap();
    path
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
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

#[cfg(unix)]
fn fake_jj(dir: &Path, snapshot: &[u8], mode: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("jj");
    let snapshot_path = dir.join("snapshot.bin");
    fs::write(&snapshot_path, snapshot).unwrap();
    let script_text = format!(
        "#!/bin/sh\nset -eu\nlog=\"${{JCW_JJ_LOG:-/dev/null}}\"\nprintf '<%s>\\n' \"$0\" >> \"$log\"\nfor argument do printf '<%s>\\n' \"$argument\" >> \"$log\"; done\nif [ \"{mode}\" = fail ]; then echo fake-jj-error >&2; exit 23; fi\ncat \"$JCW_SNAPSHOT\"\n"
    );
    fs::write(&script, script_text).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn prepare_materializes_secure_workspace_without_mutating_source() {
    let root = unique_temp_dir("success");
    fs::create_dir(root.join(".jj")).unwrap();
    let source_path = root.join("file with spaces.snapshot");
    let source = b"prefix\r\n<<<<<<< opening\r\n+++++++ first\r\nleft\r\n------- base\r\n\r\n+++++++ empty\r\n>>>>>>> closing";
    fs::write(&source_path, source).unwrap();
    let fake_bin = root.join("bin");
    fs::create_dir(&fake_bin).unwrap();
    let snapshot = b"prefix\r\n<<<<<<< opening\r\n+++++++ first\r\nleft\r\n------- base\r\n\r\n+++++++ empty\r\n>>>>>>> closing";
    fake_jj(&fake_bin, snapshot, "ok");
    let output_dir = root.join("output parent");
    fs::create_dir(&output_dir).unwrap();
    let log = root.join("jj argv.log");
    let output = Command::new(binary())
        .current_dir(&root)
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("JCW_JJ_LOG", &log)
        .env("JCW_SNAPSHOT", fake_bin.join("snapshot.bin"))
        .args([
            std::ffi::OsString::from("prepare"),
            std::ffi::OsString::from("--file"),
            source_path.file_name().unwrap().to_owned(),
            std::ffi::OsString::from("--output-dir"),
            output_dir.as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let workspace = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert!(workspace.starts_with(fs::canonicalize(&output_dir).unwrap()));
    assert_eq!(fs::read(&source_path).unwrap(), source);
    assert_eq!(fs::read(workspace.join("source")).unwrap(), source);
    assert_eq!(
        fs::read(workspace.join("resolved")).unwrap(),
        b"prefix\r\nleft"
    );
    assert_eq!(
        fs::read(workspace.join("regions/region-000/term-000.term")).unwrap(),
        b"left"
    );
    assert_eq!(
        fs::read(workspace.join("regions/region-000/term-001.term")).unwrap(),
        b""
    );
    let manifest = String::from_utf8(fs::read(workspace.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest.contains("\"schema_version\": 1"));
    assert!(manifest.contains("\"region_count\": 1"));
    assert!(manifest.contains("\"term_count\": 3"));
    assert!(manifest.contains("\"logical_length\": 0"));
    assert!(manifest.contains("\"synthetic_separator_eol_removed\": true"));
    let invocation = String::from_utf8(fs::read(log).unwrap()).unwrap();
    assert!(invocation.contains(
        "<--no-pager>\n<--config>\n<ui.conflict-marker-style=snapshot>\n<file>\n<show>\n<--revision>\n<@>\n<-->\n<file with spaces.snapshot>\n"
    ));
    assert!(workspace.join("regions/region-000/term-001.term").is_file());
    assert_eq!(
        fs::metadata(&workspace).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn prepare_reports_jj_failure_and_does_not_create_workspace() {
    let root = unique_temp_dir("failure");
    fs::create_dir(root.join(".jj")).unwrap();
    fs::write(root.join("source"), b"source").unwrap();
    let fake_bin = root.join("bin");
    fs::create_dir(&fake_bin).unwrap();
    fake_jj(&fake_bin, b"unused", "fail");
    let output_dir = root.join("output");
    fs::create_dir(&output_dir).unwrap();
    let output = Command::new(binary())
        .current_dir(&root)
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("JCW_SNAPSHOT", fake_bin.join("snapshot.bin"))
        .args([
            std::ffi::OsString::from("prepare"),
            std::ffi::OsString::from("--file"),
            std::ffi::OsString::from("source"),
            std::ffi::OsString::from("--output-dir"),
            output_dir.as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .contains("external command `jj --no-pager --config ui.conflict-marker-style=snapshot")
    );
    assert!(stderr.contains("status 23"));
    assert!(stderr.contains("fake-jj-error"));
    assert!(output.stdout.is_empty());
    let entries = fs::read_dir(&output_dir).unwrap().count();
    assert_eq!(entries, 0, "JJ failure must not create a workspace");
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn prepare_rejects_malformed_jj_snapshot_without_creating_workspace() {
    let root = unique_temp_dir("malformed-snapshot");
    fs::create_dir(root.join(".jj")).unwrap();
    fs::write(root.join("source"), b"source").unwrap();
    let fake_bin = root.join("bin");
    fs::create_dir(&fake_bin).unwrap();
    fake_jj(&fake_bin, b"not a snapshot", "ok");
    let output_dir = root.join("output");
    fs::create_dir(&output_dir).unwrap();
    let output = Command::new(binary())
        .current_dir(&root)
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("JCW_SNAPSHOT", fake_bin.join("snapshot.bin"))
        .args([
            std::ffi::OsString::from("prepare"),
            std::ffi::OsString::from("--file"),
            std::ffi::OsString::from("source"),
            std::ffi::OsString::from("--output-dir"),
            output_dir.as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("JJ snapshot output for `source` could not be parsed"));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read_dir(&output_dir).unwrap().count(), 0);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn prepare_reports_missing_jj_without_creating_workspace() {
    let root = unique_temp_dir("missing-jj");
    fs::create_dir(root.join(".jj")).unwrap();
    fs::write(root.join("source"), b"source").unwrap();
    let empty_path = root.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let output_dir = root.join("output");
    fs::create_dir(&output_dir).unwrap();
    let output = Command::new(binary())
        .current_dir(&root)
        .env("PATH", &empty_path)
        .args([
            std::ffi::OsString::from("prepare"),
            std::ffi::OsString::from("--file"),
            std::ffi::OsString::from("source"),
            std::ffi::OsString::from("--output-dir"),
            output_dir.as_os_str().to_owned(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("could not start JJ"));
    assert!(stderr.contains("No such file or directory"));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read_dir(&output_dir).unwrap().count(), 0);
    fs::remove_dir_all(root).unwrap();
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
            "  jcw prepare --file FILE [--output-dir DIR]\n",
            "  jcw apply --resolved-file FILE [--manifest FILE] [--write]\n",
            "  jcw --help\n",
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
