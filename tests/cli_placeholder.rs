use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryKind {
    Directory,
    Regular,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeEntry {
    kind: EntryKind,
    target: Option<OsString>,
    bytes: Option<Vec<u8>>,
    modified: Option<(u64, u32)>,
    mode: u32,
}

/// Snapshot every directory entry without following symlinks. Any traversal,
/// metadata, link, or regular-file read error is a test failure rather than a
/// silently incomplete snapshot.
fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, TreeEntry>) {
        let entries = fs::read_dir(path).unwrap_or_else(|error| {
            panic!(
                "could not read snapshot directory `{}`: {error}",
                path.display()
            )
        });
        for entry in entries {
            let entry =
                entry.unwrap_or_else(|error| panic!("could not read directory entry: {error}"));
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or_else(|error| panic!("snapshot path is outside root: {error}"))
                .to_owned();
            let metadata = fs::symlink_metadata(&path).unwrap_or_else(|error| {
                panic!(
                    "could not inspect snapshot entry `{}`: {error}",
                    path.display()
                )
            });
            let file_type = metadata.file_type();
            let kind = if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_file() {
                EntryKind::Regular
            } else if file_type.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::Other
            };
            let target = if kind == EntryKind::Symlink {
                Some(
                    fs::read_link(&path)
                        .unwrap_or_else(|error| {
                            panic!("could not read symlink `{}`: {error}", path.display())
                        })
                        .into_os_string(),
                )
            } else {
                None
            };
            let bytes = if kind == EntryKind::Regular {
                Some(fs::read(&path).unwrap_or_else(|error| {
                    panic!("could not read snapshot file `{}`: {error}", path.display())
                }))
            } else {
                None
            };
            let modified_time = metadata.modified().unwrap_or_else(|error| {
                panic!(
                    "could not read modification time for `{}`: {error}",
                    path.display()
                )
            });
            let modified_duration =
                modified_time
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_else(|error| {
                        panic!(
                            "modification time for `{}` predates UNIX epoch: {error}",
                            path.display()
                        )
                    });
            let modified = Some((
                modified_duration.as_secs(),
                modified_duration.subsec_nanos(),
            ));
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0;
            snapshot.insert(
                relative,
                TreeEntry {
                    kind: kind.clone(),
                    target,
                    bytes,
                    modified,
                    mode,
                },
            );
            if kind == EntryKind::Directory {
                visit(root, &path, snapshot);
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn binary() -> PathBuf {
    env::var_os("CARGO_BIN_EXE_jcw")
        .map(PathBuf::from)
        .or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_owned))
                .map(|target_debug| {
                    target_debug.join(if cfg!(windows) { "jcw.exe" } else { "jcw" })
                })
        })
        .expect("Cargo built the jcw binary")
}

fn run(args: &[&str]) -> Output {
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

#[test]
fn help_succeeds_and_no_command_is_a_stable_error() {
    let corpus = Path::new("docs/test-corpus");
    let before = snapshot_tree(corpus);
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert_eq!(
        String::from_utf8_lossy(&help.stdout),
        jj_conflict_workspace::USAGE
    );
    assert_eq!(help.stderr, b"");

    let missing = run(&[]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(missing.stdout, b"");
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .starts_with("error: missing command; choose `prepare` or `apply`\n")
    );
    assert_eq!(snapshot_tree(corpus), before);
}

#[cfg(feature = "test-support")]
mod native_fake_jj_tests {
    use super::*;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::ffi::OsStr;

    fn fake_binary() -> PathBuf {
        env::var_os("CARGO_BIN_EXE_jcw_fake_jj")
            .map(PathBuf::from)
            .or_else(|| {
                env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_owned))
                    .map(|target_debug| {
                        target_debug.join(if cfg!(windows) {
                            "jcw-fake-jj.exe"
                        } else {
                            "jcw-fake-jj"
                        })
                    })
            })
            .expect("Cargo built jcw-fake-jj with test-support")
    }

    fn native_path(entries: impl IntoIterator<Item = PathBuf>) -> OsString {
        env::join_paths(entries).expect("test paths are valid native PATH entries")
    }

    fn install_fake_jj(control: &Path) -> PathBuf {
        let directory = control.join("fake-bin");
        fs::create_dir_all(&directory).unwrap();
        let name = if cfg!(windows) { "jj.exe" } else { "jj" };
        let target = directory.join(name);
        fs::copy(fake_binary(), &target).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        }
        directory
    }

    fn test_path(fake_bin: &Path) -> OsString {
        let mut entries = vec![fake_bin.to_owned()];
        if let Some(path) = env::var_os("PATH") {
            entries.extend(env::split_paths(&path));
        }
        native_path(entries)
    }

    fn decode_percent(bytes: &[u8]) -> Vec<u8> {
        fn hex(value: u8) -> Option<u8> {
            match value {
                b'0'..=b'9' => Some(value - b'0'),
                b'A'..=b'F' => Some(value - b'A' + 10),
                _ => None,
            }
        }
        let mut result = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                assert!(
                    index + 2 < bytes.len(),
                    "strict path codec: incomplete percent escape"
                );
                let high = hex(bytes[index + 1]).unwrap_or_else(|| {
                    panic!(
                        "strict path codec: percent escape has non-uppercase hex digit {:02X}",
                        bytes[index + 1]
                    )
                });
                let low = hex(bytes[index + 2]).unwrap_or_else(|| {
                    panic!(
                        "strict path codec: percent escape has non-uppercase hex digit {:02X}",
                        bytes[index + 2]
                    )
                });
                result.push(high * 16 + low);
                index += 3;
            } else {
                result.push(bytes[index]);
                index += 1;
            }
        }
        result
    }

    fn decode_path(output: &[u8]) -> PathBuf {
        assert!(output.ends_with(b"\n"), "prepare output must end in one LF");
        assert!(output.len() > 1, "prepare output must contain a path");
        assert!(!output[..output.len() - 1].contains(&b'\n'));
        assert!(!output[..output.len() - 1].contains(&b'\r'));
        let encoded = &output[..output.len() - 1];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            return PathBuf::from(OsString::from_vec(decode_percent(encoded)));
        }
        #[cfg(not(unix))]
        {
            let text = String::from_utf8(encoded.to_vec()).unwrap();
            let decoded = decode_percent(text.as_bytes());
            return PathBuf::from(String::from_utf8(decoded).unwrap());
        }
    }

    fn decode_argument_log(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut arguments = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            assert!(bytes.len() - index >= 4, "truncated argument length");
            let length = u32::from_be_bytes(bytes[index..index + 4].try_into().unwrap()) as usize;
            index += 4;
            assert!(length <= bytes.len() - index, "truncated argument payload");
            arguments.push(bytes[index..index + length].to_vec());
            index += length;
        }
        arguments
    }

    fn native_bytes(value: &OsStr) -> Vec<u8> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return value.as_bytes().to_vec();
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            return value
                .encode_wide()
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
        }
        #[cfg(not(any(unix, windows)))]
        value.to_string_lossy().as_bytes().to_vec()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = unique_temp_dir(name);
        let repository = base.join("repository");
        let control = base.join("control");
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&control).unwrap();
        fs::create_dir(repository.join(".jj")).unwrap();
        (base, repository, control)
    }

    fn write_snapshot(control: &Path, bytes: &[u8], name: &str) -> PathBuf {
        let path = control.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn clear_protocol_environment(command: &mut Command) {
        for name in [
            "JCW_TEST_FAKE_JJ_SNAPSHOT",
            "JCW_TEST_FAKE_JJ_ARGV_LOG",
            "JCW_TEST_FAKE_JJ_STDERR_FILE",
            "JCW_TEST_FAKE_JJ_EXIT",
            "JCW_TEST_FAKE_JJ_MUTATE_SOURCE",
            "JCW_TEST_FAKE_JJ_REPLACE_SOURCE",
        ] {
            command.env_remove(name);
        }
    }

    fn run_prepare(
        repository: &Path,
        relative_source: &Path,
        output_dir: &Path,
        control: &Path,
        snapshot: Option<&Path>,
        action: Option<&str>,
        exit: Option<&str>,
        stderr_file: Option<&Path>,
        argv_log: Option<&Path>,
    ) -> Output {
        let fake_bin = install_fake_jj(control);
        let mut command = Command::new(binary());
        command
            .current_dir(repository)
            .env("PATH", test_path(&fake_bin))
            .arg("prepare")
            .arg("--file")
            .arg(relative_source.as_os_str())
            .arg("--output-dir")
            .arg(output_dir.as_os_str());
        clear_protocol_environment(&mut command);
        if let Some(path) = snapshot {
            command.env("JCW_TEST_FAKE_JJ_SNAPSHOT", path);
        }
        if let Some(path) = argv_log {
            command.env("JCW_TEST_FAKE_JJ_ARGV_LOG", path);
        }
        if let Some(path) = stderr_file {
            command.env("JCW_TEST_FAKE_JJ_STDERR_FILE", path);
        }
        if let Some(action) = action {
            if action == "mutate-bytes" {
                command.env("JCW_TEST_FAKE_JJ_MUTATE_SOURCE", action);
            } else {
                command.env("JCW_TEST_FAKE_JJ_REPLACE_SOURCE", action);
            }
        }
        if let Some(exit) = exit {
            command.env("JCW_TEST_FAKE_JJ_EXIT", exit);
        }
        command.output().unwrap()
    }

    fn simple_snapshot() -> &'static [u8] {
        b"prefix\n<<<<<<< opening\n+++++++ side\nleft\n------- base\nright\n>>>>>>> closing\nsuffix\n"
    }

    fn complex_snapshot() -> &'static [u8] {
        b"prefix\r\n<<<<<<< opening\r\n+++++++ c\xC3\xB4t\xC3\xA9\r\nleft\r\n------- base\r\nbase\r\n+++++++ empty\r\n------- repeated\r\nbase2\r\n+++++++ marker\r\n<<<<<< payload\r\n>>>>>>> closing\r\nbetween\n<<<<<<< second\n+++++++ one\none\n------- base\nold\n+++++++ deleted\n------- base2\nold2\n+++++++ five\nfive\n>>>>>>> closing"
    }

    fn prepare_simple(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (base, repository, control) = fixture(name);
        let source = repository.join("source file %");
        fs::write(&source, simple_snapshot()).unwrap();
        let snapshot = write_snapshot(&control, simple_snapshot(), "snapshot.bin");
        let output_dir = base.join("workspace output");
        fs::create_dir(&output_dir).unwrap();
        let relative = source.strip_prefix(&repository).unwrap().to_owned();
        let output = run_prepare(
            &repository,
            &relative,
            &output_dir,
            &control,
            Some(&snapshot),
            None,
            None,
            None,
            None,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let workspace = decode_path(&output.stdout);
        (base, source, workspace)
    }

    #[test]
    fn native_fake_jj_protocol_records_native_arguments_and_separates_raw_streams() {
        let base = unique_temp_dir("fake-protocol");
        let snapshot = base.join("snapshot\n%.bin");
        let diagnostics = base.join("diagnostics.bin");
        let log = base.join("argv.bin");
        fs::write(&snapshot, b"snapshot\0\xff").unwrap();
        fs::write(&diagnostics, b"diagnostic\0\xfe").unwrap();
        let special = OsString::from("space % newline\n");
        let mut command = Command::new(fake_binary());
        command
            .current_dir(&base)
            .args([
                OsString::from("--no-pager"),
                OsString::from("--config"),
                OsString::from("ui.conflict-marker-style=snapshot"),
                special.clone(),
            ])
            .env("JCW_TEST_FAKE_JJ_SNAPSHOT", &snapshot)
            .env("JCW_TEST_FAKE_JJ_STDERR_FILE", &diagnostics)
            .env("JCW_TEST_FAKE_JJ_ARGV_LOG", &log);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            command.arg(OsString::from_vec(b"invalid-\xff".to_vec()));
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"snapshot\0\xff");
        assert_eq!(output.stderr, b"diagnostic\0\xfe");
        let expected_args = command_args_for_protocol(&special);
        assert_eq!(decode_argument_log(&fs::read(log).unwrap()), expected_args);

        let invalid = Command::new(fake_binary())
            .current_dir(&base)
            .env("JCW_TEST_FAKE_JJ_MUTATE_SOURCE", "invalid-secret-action")
            .env("JCW_SECRET_TEST_VALUE", base.as_os_str())
            .output()
            .unwrap();
        assert!(!invalid.status.success());
        assert!(
            !String::from_utf8_lossy(&invalid.stderr).contains(base.to_string_lossy().as_ref())
        );
        assert!(!String::from_utf8_lossy(&invalid.stderr).contains("invalid-secret-action"));
        fs::remove_dir_all(base).unwrap();
    }

    fn command_args_for_protocol(special: &OsString) -> Vec<Vec<u8>> {
        let mut values = vec![
            OsString::from("--no-pager"),
            OsString::from("--config"),
            OsString::from("ui.conflict-marker-style=snapshot"),
            special.clone(),
        ];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            values.push(OsString::from_vec(b"invalid-\xff".to_vec()));
        }
        values
            .iter()
            .map(|value| native_bytes(value.as_os_str()))
            .collect()
    }

    #[test]
    fn prepare_end_to_end_adversarial_fixture() {
        let (base, repository, control) = fixture("prepare-adversarial");
        let source = repository.join("nested").join("source file-é.snapshot");
        fs::create_dir(source.parent().unwrap()).unwrap();
        fs::write(&source, complex_snapshot()).unwrap();
        let before = snapshot_tree(&repository);
        let snapshot = write_snapshot(&control, complex_snapshot(), "complex.snapshot");
        let log = control.join("argv.log");
        let output_dir = base.join("output parent %");
        fs::create_dir(&output_dir).unwrap();
        let relative = source.strip_prefix(&repository).unwrap().to_owned();
        let output = run_prepare(
            &repository,
            &relative,
            &output_dir,
            &control,
            Some(&snapshot),
            None,
            None,
            None,
            Some(&log),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let workspace = decode_path(&output.stdout);
        assert!(workspace.starts_with(fs::canonicalize(&output_dir).unwrap()));
        assert_eq!(fs::read(&source).unwrap(), complex_snapshot());
        assert_eq!(snapshot_tree(&repository), before);
        assert_eq!(
            fs::read(workspace.join("source")).unwrap(),
            complex_snapshot()
        );
        let seed_region_0 = b"JCW-UNRESOLVED-CONFLICT-REGION-000: replace this line with the final content for this conflict, or delete the line to drop the content. Terms: regions/region-000/term-000.term, regions/region-000/term-001.term, regions/region-000/term-002.term, regions/region-000/term-003.term, regions/region-000/term-004.term\r\n";
        let seed_region_1 = b"JCW-UNRESOLVED-CONFLICT-REGION-001: replace this line with the final content for this conflict, or delete the line to drop the content. Terms: regions/region-001/term-000.term, regions/region-001/term-001.term, regions/region-001/term-002.term, regions/region-001/term-003.term, regions/region-001/term-004.term";
        assert_eq!(
            fs::read(workspace.join("resolved")).unwrap(),
            [
                b"prefix\r\n".as_slice(),
                seed_region_0,
                b"between\n".as_slice(),
                seed_region_1,
            ]
            .concat()
        );
        let expected_terms: [[&[u8]; 5]; 2] = [
            [
                b"left\r\n",
                b"base\r\n",
                b"",
                b"base2\r\n",
                b"<<<<<< payload\r\n",
            ],
            [b"one", b"old", b"", b"old2", b"five"],
        ];
        for (region, terms) in expected_terms.iter().enumerate() {
            for (term, expected) in terms.iter().enumerate() {
                let artifact =
                    workspace.join(format!("regions/region-{region:03}/term-{term:03}.term"));
                assert_eq!(
                    fs::read(&artifact).unwrap(),
                    *expected,
                    "unexpected artifact bytes for region {region} term {term}"
                );
            }
        }
        let manifest_bytes = fs::read(workspace.join("manifest.json")).unwrap();
        let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
        let manifest_object = manifest.as_object().unwrap();
        assert_eq!(
            manifest_object.keys().collect::<Vec<_>>(),
            [
                "marker",
                "region_count",
                "regions",
                "schema_version",
                "source",
                "source_length",
                "source_sha256",
            ]
        );
        assert_eq!(manifest["schema_version"], Value::from(2));
        assert_eq!(
            manifest["source_length"],
            Value::from(complex_snapshot().len())
        );
        assert_eq!(manifest["region_count"], Value::from(2));
        let source_canonical = fs::canonicalize(&source).unwrap();
        let source_object = manifest["source"].as_object().unwrap();
        assert_eq!(
            source_object.keys().collect::<Vec<_>>(),
            [
                "canonical_path",
                "canonical_path_bytes_hex",
                "repository_relative",
                "repository_relative_bytes_hex",
            ]
        );
        assert_eq!(
            source_object["canonical_path"],
            Value::from(source_canonical.to_string_lossy().into_owned())
        );
        assert_eq!(
            source_object["canonical_path_bytes_hex"],
            Value::from(
                native_bytes(source_canonical.as_os_str())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            )
        );
        assert_eq!(
            source_object["repository_relative"],
            Value::from("nested/source file-é.snapshot")
        );
        assert_eq!(
            source_object["repository_relative_bytes_hex"],
            Value::from(
                native_bytes(relative.as_os_str())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            )
        );
        assert_eq!(
            manifest["source_sha256"],
            Value::from(sha256_hex(complex_snapshot()))
        );
        let marker = manifest["marker"].as_object().unwrap();
        assert_eq!(
            marker.keys().collect::<Vec<_>>(),
            ["outer_marker_width", "section_marker_width", "style"]
        );
        assert_eq!(marker["style"], Value::from("Snapshot"));
        assert_eq!(marker["outer_marker_width"], Value::from(7));
        assert_eq!(marker["section_marker_width"], Value::from(7));

        let document = jj_conflict_workspace::parse_snapshot(complex_snapshot()).unwrap();
        let manifest_regions = manifest["regions"].as_array().unwrap();
        assert_eq!(manifest_regions.len(), document.regions.len());
        assert_eq!(manifest_regions.len(), 2);
        let expected_terms: [[&[u8]; 5]; 2] = [
            [
                b"left\r\n",
                b"base\r\n",
                b"",
                b"base2\r\n",
                b"<<<<<< payload\r\n",
            ],
            [b"one", b"old", b"", b"old2", b"five"],
        ];
        let expected_labels = [
            [" côté", " base", " empty", " repeated", " marker"],
            [" one", " base", " deleted", " base2", " five"],
        ];
        for (region_index, (manifest_region, parsed_region)) in
            manifest_regions.iter().zip(&document.regions).enumerate()
        {
            let region_object = manifest_region.as_object().unwrap();
            assert_eq!(
                region_object.keys().collect::<Vec<_>>(),
                [
                    "region_index",
                    "seed",
                    "source_range",
                    "term_count",
                    "terms"
                ]
            );
            assert_eq!(region_object["region_index"], Value::from(region_index));
            assert_eq!(
                region_object["seed"],
                Value::from(
                    std::str::from_utf8(if region_index == 0 {
                        seed_region_0
                    } else {
                        seed_region_1
                    })
                    .unwrap()
                    .to_owned()
                )
            );
            assert_eq!(
                region_object["term_count"],
                Value::from(parsed_region.terms.len())
            );
            let range = region_object["source_range"].as_object().unwrap();
            assert_eq!(range.keys().collect::<Vec<_>>(), ["end", "start"]);
            assert_eq!(
                range["start"],
                Value::from(parsed_region.source_range.start)
            );
            assert_eq!(range["end"], Value::from(parsed_region.source_range.end));

            let terms = region_object["terms"].as_array().unwrap();
            assert_eq!(terms.len(), parsed_region.terms.len());
            assert_eq!(terms.len(), expected_terms[region_index].len());
            for (ordinal, ((term_object, parsed_term), expected_bytes)) in terms
                .iter()
                .zip(&parsed_region.terms)
                .zip(expected_terms[region_index])
                .enumerate()
            {
                let term = term_object.as_object().unwrap();
                assert_eq!(
                    term.keys().collect::<Vec<_>>(),
                    [
                        "artifact_path",
                        "kind",
                        "label",
                        "logical_final_newline",
                        "logical_length",
                        "ordinal",
                        "sha256",
                        "synthetic_separator_eol_removed",
                    ]
                );
                let artifact_path =
                    format!("regions/region-{region_index:03}/term-{ordinal:03}.term");
                assert_eq!(term["ordinal"], Value::from(ordinal));
                assert_eq!(term["kind"], Value::from(parsed_term.kind.to_string()));
                assert_eq!(
                    term["label"],
                    Value::from(expected_labels[region_index][ordinal])
                );
                assert_eq!(term["logical_length"], Value::from(expected_bytes.len()));
                assert_eq!(
                    term["logical_final_newline"],
                    Value::from(expected_bytes.last() == Some(&b'\n'))
                );
                assert_eq!(
                    term["synthetic_separator_eol_removed"],
                    Value::from(parsed_term.synthetic_separator_eol_removed)
                );
                assert_eq!(term["artifact_path"], Value::from(artifact_path.clone()));
                assert_eq!(term["sha256"], Value::from(sha256_hex(expected_bytes)));
                assert_eq!(
                    fs::read(workspace.join(&artifact_path)).unwrap(),
                    expected_bytes,
                    "unexpected artifact bytes for region {region_index} term {ordinal}"
                );
            }
        }
        let arguments = decode_argument_log(&fs::read(log).unwrap());
        let expected_arguments = [
            OsString::from("--no-pager"),
            OsString::from("--config"),
            OsString::from("ui.conflict-marker-style=snapshot"),
            OsString::from("file"),
            OsString::from("show"),
            OsString::from("--revision"),
            OsString::from("@"),
            OsString::from("--"),
            relative.as_os_str().to_owned(),
        ]
        .iter()
        .map(|value| native_bytes(value.as_os_str()))
        .collect::<Vec<_>>();
        assert_eq!(
            arguments, expected_arguments,
            "prepare must invoke JJ with the exact fixed production argv"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prepare_preserves_source_bytes_metadata_and_tree() {
        let (base, repository, control) = fixture("prepare-source-preservation");
        let source = repository.join("source with spaces-é");
        fs::write(&source, complex_snapshot()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        }
        let before = snapshot_tree(&repository);
        let snapshot = write_snapshot(&control, complex_snapshot(), "snapshot.bin");
        let output_dir = base.join("output");
        fs::create_dir(&output_dir).unwrap();
        let relative = source.strip_prefix(&repository).unwrap().to_owned();
        let output = run_prepare(
            &repository,
            &relative,
            &output_dir,
            &control,
            Some(&snapshot),
            None,
            None,
            None,
            None,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(snapshot_tree(&repository), before);
        assert_eq!(fs::read(&source).unwrap(), complex_snapshot());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn apply_rejects_untouched_seed_and_accepts_replaced_placeholders() {
        let (base, source, workspace) = prepare_simple("apply-placeholder-seed");
        let source_before = fs::read(&source).unwrap();

        // The untouched seed is marker-free and preserves outside bytes, yet
        // apply must refuse it with an actionable region-naming message.
        let rejected = run_apply(&workspace, false);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(rejected.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.starts_with("error: "), "unexpected stderr: {stderr}");
        assert!(stderr.contains("JCW"), "unexpected stderr: {stderr}");
        assert!(stderr.contains("region 0"), "unexpected stderr: {stderr}");
        assert!(
            stderr.contains("regions/region-000/term-000.term"),
            "unexpected stderr: {stderr}"
        );
        assert!(
            stderr.contains("regions/region-000/term-001.term"),
            "unexpected stderr: {stderr}"
        );
        assert_eq!(fs::read(&source).unwrap(), source_before);

        // Replacing every placeholder line with content makes the same apply
        // succeed.
        fs::write(workspace.join("resolved"), b"prefix\nchanged\nsuffix\n").unwrap();
        let accepted = run_apply(&workspace, false);
        assert!(
            accepted.status.success(),
            "{}",
            String::from_utf8_lossy(&accepted.stderr)
        );
        assert!(accepted.stderr.is_empty());
        assert!(String::from_utf8_lossy(&accepted.stdout).contains("+changed"));
        assert_eq!(fs::read(&source).unwrap(), source_before);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn readme_workflow_contract_smoke() {
        let (base, repository, control) = fixture("apply-dry-write");
        let source = repository.join("source file %");
        fs::write(&source, simple_snapshot()).unwrap();
        #[cfg(unix)]
        {
            use filetime::{FileTime, set_file_mtime};
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&source, fs::Permissions::from_mode(0o741)).unwrap();
            set_file_mtime(
                &source,
                FileTime::from_unix_time(1_700_000_000, 123_456_789),
            )
            .unwrap();
        }
        let before_source = fs::read(&source).unwrap();
        let snapshot = write_snapshot(&control, simple_snapshot(), "snapshot.bin");
        let output_dir = base.join("workspace output");
        fs::create_dir(&output_dir).unwrap();
        let relative_source = source.strip_prefix(&repository).unwrap().to_owned();
        let fake_bin = install_fake_jj(&control);
        let mut prepare = Command::new(binary());
        prepare
            .current_dir(&repository)
            .env("PATH", test_path(&fake_bin))
            .arg("prepare")
            .arg("--file")
            .arg(relative_source.as_os_str())
            .arg("--output-dir")
            .arg(output_dir.as_os_str());
        clear_protocol_environment(&mut prepare);
        prepare.env("JCW_TEST_FAKE_JJ_SNAPSHOT", &snapshot);
        let prepared = prepare.output().unwrap();
        assert_eq!(prepared.status.code(), Some(0));
        assert!(prepared.stderr.is_empty());
        let workspace = decode_path(&prepared.stdout);
        assert!(workspace.is_absolute());
        assert_eq!(
            prepared.stdout,
            format!(
                "{}\n",
                jj_conflict_workspace::encode_path_for_output(&workspace)
            )
            .into_bytes()
        );
        assert!(workspace.starts_with(fs::canonicalize(&output_dir).unwrap()));
        assert_eq!(fs::read(&source).unwrap(), before_source);
        fs::write(workspace.join("resolved"), b"prefix\nchanged\nsuffix\n").unwrap();
        let before_repository = snapshot_tree(&repository);
        let before_workspace = snapshot_tree(&workspace);
        #[cfg(all(
            unix,
            any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            )
        ))]
        let before_write_source = before_repository
            .get(Path::new("source file %"))
            .expect("source must be present in the repository snapshot")
            .clone();
        let dry_run = run_apply(&workspace, false);
        assert!(
            dry_run.status.success(),
            "{}",
            String::from_utf8_lossy(&dry_run.stderr)
        );
        assert!(dry_run.stderr.is_empty());
        let expected_dry_stdout = format!(
            "Proposed changes for source `{}`:\n\
--- source\n\
+++ resolved\n\
@@ -2,6 +2,1 @@ bytes [7..76)\n\
-<<<<<<< opening\n\
-+++++++ side\n\
-left\n\
-------- base\n\
-right\n\
->>>>>>> closing\n\
+changed\n\
No files were modified (dry-run).\n",
            jj_conflict_workspace::encode_path_for_output(&fs::canonicalize(&source).unwrap()),
        );
        assert_eq!(dry_run.stdout, expected_dry_stdout.into_bytes());
        assert_eq!(fs::read(&source).unwrap(), simple_snapshot());
        assert_eq!(snapshot_tree(&workspace), before_workspace);
        assert_eq!(snapshot_tree(&repository), before_repository);

        let written = run_apply(&workspace, true);
        #[cfg(all(
            unix,
            any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            )
        ))]
        {
            assert!(
                written.status.success(),
                "{}",
                String::from_utf8_lossy(&written.stderr)
            );
            assert!(written.stderr.is_empty());
            assert_eq!(
                String::from_utf8(written.stdout).unwrap(),
                format!(
                    "Applied 1 change(s) to `{}` ({} bytes -> {} bytes).\n",
                    jj_conflict_workspace::encode_path_for_output(
                        &fs::canonicalize(&source).unwrap()
                    ),
                    simple_snapshot().len(),
                    b"prefix\nchanged\nsuffix\n".len()
                )
            );
            assert_eq!(fs::read(&source).unwrap(), b"prefix\nchanged\nsuffix\n");
            assert_eq!(snapshot_tree(&workspace), before_workspace);
            let after_write_source = snapshot_tree(&repository)
                .remove(Path::new("source file %"))
                .expect("source must be present after write");
            assert_eq!(after_write_source.mode, before_write_source.mode);
            assert_eq!(after_write_source.modified, before_write_source.modified);
            assert!(workspace.join("manifest.json").is_file());
        }
        #[cfg(not(all(
            unix,
            any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            )
        )))]
        {
            assert_eq!(written.status.code(), Some(1));
            assert!(written.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&written.stderr);
            assert!(
                stderr.contains("identity-safe replacement unsupported"),
                "unexpected write diagnostic: {stderr}"
            );
            assert_eq!(fs::read(&source).unwrap(), simple_snapshot());
            assert_eq!(snapshot_tree(&workspace), before_workspace);
            assert_eq!(snapshot_tree(&repository), before_repository);
        }
        fs::remove_dir_all(base).unwrap();
    }

    fn run_apply(workspace: &Path, write: bool) -> Output {
        run_apply_path(&workspace.join("resolved"), write)
    }

    fn run_apply_path(resolved: &Path, write: bool) -> Output {
        let mut command = Command::new(binary());
        command.arg("apply").arg("--resolved-file").arg(resolved);
        if write {
            command.arg("--write");
        }
        command.output().unwrap()
    }

    fn rewrite_manifest(workspace: &Path, edit: impl FnOnce(&mut Value)) {
        let path = workspace.join("manifest.json");
        let bytes = fs::read(&path).unwrap();
        let mut manifest: Value = serde_json::from_slice(&bytes).unwrap();
        edit(&mut manifest);
        fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    fn assert_rejected_apply_case(
        name: &str,
        expected_fragments: &[&str],
        tamper: impl FnOnce(&Path, &Path) -> PathBuf,
    ) {
        let (base, source, workspace) = prepare_simple(name);
        let repository = source.parent().unwrap().to_owned();
        let source_before = fs::read(&source).unwrap();
        let repository_before = snapshot_tree(&repository);
        let apply_workspace = tamper(&base, &workspace);
        let workspace_before = snapshot_tree(&workspace);

        let output = run_apply_path(&apply_workspace.join("resolved"), true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{name}: unexpected status: stdout={:?}, stderr={:?}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.is_empty(), "{name}: rejection wrote stdout");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("error: "),
            "{name}: non-actionable stderr: {stderr}"
        );
        for fragment in expected_fragments {
            assert!(
                stderr.contains(fragment),
                "{name}: stderr missing `{fragment}`: {stderr}"
            );
        }
        assert_eq!(
            fs::read(&source).unwrap(),
            source_before,
            "{name}: source bytes changed"
        );
        assert_eq!(
            snapshot_tree(&repository),
            repository_before,
            "{name}: repository changed"
        );
        assert_eq!(
            snapshot_tree(&workspace),
            workspace_before,
            "{name}: workspace was not retained exactly"
        );
        assert!(
            apply_workspace.join("resolved").exists()
                || apply_workspace.join("resolved").is_symlink(),
            "{name}: workspace path was not retained"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn apply_subprocess_rejection_tampering_matrix() {
        assert_rejected_apply_case(
            "apply-tamper-artifact-digest",
            &["invalid manifest", "SHA-256", "term-000.term"],
            |_, workspace| {
                rewrite_manifest(workspace, |manifest| {
                    manifest["regions"][0]["terms"][0]["sha256"] = Value::String("00".repeat(32));
                });
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-tamper-artifact-length",
            &["invalid manifest", "artifact length", "term-000.term"],
            |_, workspace| {
                rewrite_manifest(workspace, |manifest| {
                    let length = manifest["regions"][0]["terms"][0]["logical_length"]
                        .as_u64()
                        .unwrap();
                    manifest["regions"][0]["terms"][0]["logical_length"] = Value::from(length + 1);
                });
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-tamper-artifact-bytes",
            &["invalid manifest", "SHA-256", "term-000.term"],
            |_, workspace| {
                fs::write(
                    workspace.join("regions/region-000/term-000.term"),
                    b"LEFT\n",
                )
                .unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-missing-artifact",
            &["path unavailable", "term-000.term"],
            |_, workspace| {
                fs::remove_file(workspace.join("regions/region-000/term-000.term")).unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-unreadable-artifact",
            &["path unavailable", "not regular", "term-000.term"],
            |_, workspace| {
                let artifact = workspace.join("regions/region-000/term-000.term");
                fs::remove_file(&artifact).unwrap();
                fs::create_dir(&artifact).unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-wrong-repository-relative",
            &["repository-relative", "wrong-relative"],
            |_, workspace| {
                rewrite_manifest(workspace, |manifest| {
                    manifest["source"]["repository_relative"] =
                        Value::String("wrong-relative".into());
                    manifest["source"]["repository_relative_bytes_hex"] = Value::String(
                        native_bytes(Path::new("wrong-relative").as_os_str())
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                    );
                });
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-outside-region-edit",
            &["outside-region"],
            |_, workspace| {
                fs::write(
                    workspace.join("resolved"),
                    b"changed prefix\nleft\nsuffix\n",
                )
                .unwrap();
                workspace.to_owned()
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_subprocess_symlink_rejection_matrix() {
        use std::os::unix::fs::symlink;

        assert_rejected_apply_case(
            "apply-symlinked-manifest",
            &["path unavailable", "manifest", "symlink"],
            |base, workspace| {
                let target = base.join("manifest-target.json");
                fs::copy(workspace.join("manifest.json"), &target).unwrap();
                fs::remove_file(workspace.join("manifest.json")).unwrap();
                symlink(&target, workspace.join("manifest.json")).unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-symlinked-resolved",
            &["path unavailable", "resolved", "symlink"],
            |base, workspace| {
                let target = base.join("resolved-target");
                fs::copy(workspace.join("resolved"), &target).unwrap();
                fs::remove_file(workspace.join("resolved")).unwrap();
                symlink(&target, workspace.join("resolved")).unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-symlinked-artifact",
            &["path unavailable", "term-000.term", "symlink"],
            |base, workspace| {
                let target = base.join("artifact-target.term");
                fs::copy(workspace.join("regions/region-000/term-000.term"), &target).unwrap();
                let artifact = workspace.join("regions/region-000/term-000.term");
                fs::remove_file(&artifact).unwrap();
                symlink(&target, artifact).unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-symlinked-workspace-component",
            &["path unavailable", "regions", "symlink"],
            |base, workspace| {
                let target = base.join("regions-target");
                fs::rename(workspace.join("regions"), &target).unwrap();
                symlink(&target, workspace.join("regions")).unwrap();
                workspace.to_owned()
            },
        );
        assert_rejected_apply_case(
            "apply-symlinked-workspace-root",
            &["path unavailable", "symlink"],
            |base, workspace| {
                let alias = base.join("workspace-alias");
                symlink(workspace, &alias).unwrap();
                alias
            },
        );
    }

    #[test]
    fn apply_stale_source_rejected_without_source_mutation() {
        let (base, source, workspace) = prepare_simple("apply-stale");
        fs::write(workspace.join("resolved"), b"prefix\nchanged\nsuffix\n").unwrap();
        fs::write(&source, b"stale source bytes\n").unwrap();
        let before = fs::read(&source).unwrap();
        let output = run_apply(&workspace, true);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("stale source"));
        assert_eq!(fs::read(&source).unwrap(), before);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prepare_rejects_malformed_and_unsupported_snapshots() {
        for (name, snapshot_bytes, expected) in [
            (
                "no-conflict",
                b"not a snapshot".as_slice(),
                "no conflict found",
            ),
            (
                "unsupported",
                b"<<<<<<< opening\n+++++++ side\nx\n||||||| base\ny\n>>>>>>> closing\n".as_slice(),
                "unsupported snapshot style",
            ),
        ] {
            let (base, repository, control) = fixture(&format!("prepare-{name}"));
            let source = repository.join("source");
            fs::write(&source, b"source").unwrap();
            let before = snapshot_tree(&repository);
            let snapshot = write_snapshot(&control, snapshot_bytes, "snapshot.bin");
            let output_dir = base.join("output");
            fs::create_dir(&output_dir).unwrap();
            let output = run_prepare(
                &repository,
                Path::new("source"),
                &output_dir,
                &control,
                Some(&snapshot),
                None,
                None,
                None,
                None,
            );
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
            assert_eq!(snapshot_tree(&repository), before);
            assert_eq!(fs::read_dir(output_dir).unwrap().count(), 0);
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn prepare_source_changes_during_jj_never_create_a_workspace() {
        for action in ["mutate-bytes", "replace-identical"] {
            let (base, repository, control) = fixture(&format!("prepare-source-change-{action}"));
            let source = repository.join("source");
            fs::write(&source, simple_snapshot()).unwrap();
            let before = fs::read(&source).unwrap();
            #[cfg(unix)]
            let before_identity = {
                use std::os::unix::fs::MetadataExt;
                fs::symlink_metadata(&source).unwrap().ino()
            };
            let snapshot = write_snapshot(&control, simple_snapshot(), "snapshot.bin");
            let output_dir = base.join("output");
            fs::create_dir(&output_dir).unwrap();
            let output = run_prepare(
                &repository,
                Path::new("source"),
                &output_dir,
                &control,
                Some(&snapshot),
                Some(action),
                None,
                None,
                None,
            );
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr).unwrap();
            let encoded_source =
                jj_conflict_workspace::encode_path_for_output(&fs::canonicalize(&source).unwrap());
            assert!(stderr.contains(&format!(
                "source changed during JJ (after-jj) for `{encoded_source}`: expected identity "
            )));
            assert!(stderr.contains(", observed identity "));
            assert_eq!(stderr.matches("type=regular").count(), 2);
            assert_eq!(stderr.matches("len=").count(), 2);
            assert_eq!(stderr.matches("mode=").count(), 2);
            assert_eq!(stderr.matches("mtime=").count(), 2);
            #[cfg(unix)]
            {
                assert!(stderr.contains(",dev="));
                assert!(stderr.contains(",ino="));
            }
            assert_eq!(fs::read_dir(&output_dir).unwrap().count(), 0);
            assert!(!output_dir.join("manifest.json").exists());
            if action == "replace-identical" {
                assert_eq!(fs::read(&source).unwrap(), before);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    assert_ne!(
                        before_identity,
                        fs::symlink_metadata(&source).unwrap().ino(),
                        "replace-identical must be checked by the diagnostic identity, not bytes"
                    );
                }
            } else {
                assert_ne!(fs::read(&source).unwrap(), before);
            }
            fs::remove_dir_all(base).unwrap();
        }
    }
}
