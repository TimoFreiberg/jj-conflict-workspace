use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::error::CliError;

pub const USAGE: &str = "Usage:\n  jj-conflict-untangler prepare --file FILE [--output-dir DIR]\n  jj-conflict-untangler apply --resolved-file FILE [--manifest FILE] [--write]\n  jj-conflict-untangler --help";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareOptions {
    pub file: PathBuf,
    pub output_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyOptions {
    pub resolved_file: PathBuf,
    pub manifest: Option<PathBuf>,
    pub write: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Prepare(PrepareOptions),
    Apply(ApplyOptions),
}

fn display_os(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

fn take_value(args: &[OsString], index: &mut usize, option: &str) -> Result<PathBuf, CliError> {
    *index += 1;
    let value = args.get(*index).ok_or_else(|| CliError::MissingValue {
        option: option.to_owned(),
    })?;
    if value.is_empty() {
        return Err(CliError::EmptyArgument);
    }
    if value == OsStr::new("--help")
        || value == OsStr::new("-h")
        || value.to_string_lossy().starts_with('-')
    {
        return Err(CliError::MissingValue {
            option: option.to_owned(),
        });
    }
    Ok(PathBuf::from(value))
}

/// Parse command arguments without converting path values through UTF-8.
pub fn parse_args<I, T>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.is_empty() {
        return Err(CliError::MissingCommand);
    }
    if args
        .iter()
        .any(|arg| arg == OsStr::new("--help") || arg == OsStr::new("-h"))
    {
        return Err(CliError::HelpRequested);
    }
    if args[0].is_empty() {
        return Err(CliError::EmptyArgument);
    }

    match args[0].as_os_str() {
        value if value == OsStr::new("prepare") => parse_prepare(&args[1..]),
        value if value == OsStr::new("apply") => parse_apply(&args[1..]),
        value => Err(CliError::UnknownCommand {
            command: display_os(value),
        }),
    }
}

fn parse_prepare(args: &[OsString]) -> Result<Command, CliError> {
    let mut file = None;
    let mut output_dir = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_os_str() {
            value if value == OsStr::new("--file") => {
                if file.is_some() {
                    return Err(CliError::DuplicateOption {
                        option: "--file".into(),
                    });
                }
                file = Some(take_value(args, &mut index, "--file")?);
            }
            value if value == OsStr::new("--output-dir") => {
                if output_dir.is_some() {
                    return Err(CliError::DuplicateOption {
                        option: "--output-dir".into(),
                    });
                }
                output_dir = Some(take_value(args, &mut index, "--output-dir")?);
            }
            value
                if value == OsStr::new("--write")
                    || value == OsStr::new("--resolved-file")
                    || value == OsStr::new("--manifest") =>
            {
                return Err(CliError::IncompatibleOption {
                    option: display_os(value),
                    command: "prepare".into(),
                });
            }
            value if value.to_string_lossy().starts_with('-') => {
                return Err(CliError::UnknownOption {
                    option: display_os(value),
                });
            }
            value => {
                return Err(CliError::UnexpectedArgument {
                    argument: display_os(value),
                });
            }
        }
        index += 1;
    }
    let file = file.ok_or_else(|| CliError::MissingValue {
        option: "--file".into(),
    })?;
    Ok(Command::Prepare(PrepareOptions { file, output_dir }))
}

fn parse_apply(args: &[OsString]) -> Result<Command, CliError> {
    let mut resolved_file = None;
    let mut manifest = None;
    let mut write = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_os_str() {
            value if value == OsStr::new("--resolved-file") => {
                if resolved_file.is_some() {
                    return Err(CliError::DuplicateOption {
                        option: "--resolved-file".into(),
                    });
                }
                resolved_file = Some(take_value(args, &mut index, "--resolved-file")?);
            }
            value if value == OsStr::new("--manifest") => {
                if manifest.is_some() {
                    return Err(CliError::DuplicateOption {
                        option: "--manifest".into(),
                    });
                }
                manifest = Some(take_value(args, &mut index, "--manifest")?);
            }
            value if value == OsStr::new("--write") => {
                if write {
                    return Err(CliError::DuplicateOption {
                        option: "--write".into(),
                    });
                }
                write = true;
            }
            value if value == OsStr::new("--file") || value == OsStr::new("--output-dir") => {
                return Err(CliError::IncompatibleOption {
                    option: display_os(value),
                    command: "apply".into(),
                });
            }
            value if value.to_string_lossy().starts_with('-') => {
                return Err(CliError::UnknownOption {
                    option: display_os(value),
                });
            }
            value => {
                return Err(CliError::UnexpectedArgument {
                    argument: display_os(value),
                });
            }
        }
        index += 1;
    }
    let resolved_file = resolved_file.ok_or_else(|| CliError::MissingValue {
        option: "--resolved-file".into(),
    })?;
    Ok(Command::Apply(ApplyOptions {
        resolved_file,
        manifest,
        write,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> Result<Command, CliError> {
        parse_args(values.iter().copied())
    }

    #[test]
    fn parses_both_commands_and_defaults_write_to_false() {
        assert_eq!(
            parse(&["prepare", "--file", "a", "--output-dir", "out"]).unwrap(),
            Command::Prepare(PrepareOptions {
                file: "a".into(),
                output_dir: Some("out".into()),
            })
        );
        assert_eq!(
            parse(&["apply", "--resolved-file", "r", "--manifest", "m"]).unwrap(),
            Command::Apply(ApplyOptions {
                resolved_file: "r".into(),
                manifest: Some("m".into()),
                write: false,
            })
        );
        assert!(matches!(
            parse(&["apply", "--resolved-file", "r", "--write"]).unwrap(),
            Command::Apply(ApplyOptions { write: true, .. })
        ));
    }

    #[test]
    fn rejects_syntax_errors_deterministically() {
        assert_eq!(parse(&[]), Err(CliError::MissingCommand));
        assert_eq!(
            parse(&["prepare"]),
            Err(CliError::MissingValue {
                option: "--file".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file"]),
            Err(CliError::MissingValue {
                option: "--file".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file", "--output-dir", "out"]),
            Err(CliError::MissingValue {
                option: "--file".into()
            })
        );
        assert_eq!(
            parse(&["apply", "--resolved-file", "--write"]),
            Err(CliError::MissingValue {
                option: "--resolved-file".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file", "a", "--unknown"]),
            Err(CliError::UnknownOption {
                option: "--unknown".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file", "a", "--file", "b"]),
            Err(CliError::DuplicateOption {
                option: "--file".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file", "a", "--write"]),
            Err(CliError::IncompatibleOption {
                option: "--write".into(),
                command: "prepare".into()
            })
        );
        assert_eq!(parse(&["--help"]), Err(CliError::HelpRequested));
        assert_eq!(
            parse(&["unknown"]),
            Err(CliError::UnknownCommand {
                command: "unknown".into()
            })
        );
        assert_eq!(
            parse(&["apply", "--resolved-file", "r", "--write", "--write"]),
            Err(CliError::DuplicateOption {
                option: "--write".into()
            })
        );
        assert_eq!(
            parse(&["apply", "--resolved-file", "r", "--resolved-file", "x"]),
            Err(CliError::DuplicateOption {
                option: "--resolved-file".into()
            })
        );
        assert_eq!(
            parse(&["apply", "--resolved-file", "r", "--file", "x"]),
            Err(CliError::IncompatibleOption {
                option: "--file".into(),
                command: "apply".into()
            })
        );
        assert_eq!(
            parse(&["apply", "--manifest"]),
            Err(CliError::MissingValue {
                option: "--manifest".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file", "a", "extra"]),
            Err(CliError::UnexpectedArgument {
                argument: "extra".into()
            })
        );
        assert_eq!(
            parse(&["prepare", "--file", ""]),
            Err(CliError::EmptyArgument)
        );
    }

    #[cfg(unix)]
    #[test]
    fn retains_non_utf8_path_values() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(vec![b'f', b'i', b'l', b'e', 0xff]);
        let command = parse_args(vec![
            OsString::from("prepare"),
            OsString::from("--file"),
            path.clone(),
        ])
        .unwrap();
        assert_eq!(
            command,
            Command::Prepare(PrepareOptions {
                file: PathBuf::from(path),
                output_dir: None
            })
        );
    }
}
