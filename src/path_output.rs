//! Reversible, single-line rendering for native filesystem paths.

use std::path::Path;

/// Encode a path for diagnostics and machine-readable one-line output.
///
/// On Unix this operates on the raw path bytes. Printable ASCII other than `%`
/// is emitted literally; every other ASCII byte is emitted as `%HH`, while
/// valid UTF-8 non-ASCII sequences are preserved. On non-Unix targets the
/// native path text is used and controls and `%` are escaped.
pub fn encode_path_for_output(path: &Path) -> String {
    encode_os_str(path.as_os_str())
}

#[cfg(unix)]
fn encode_os_str(value: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    encode_bytes(value.as_bytes())
}

#[cfg(not(unix))]
fn encode_os_str(value: &std::ffi::OsStr) -> String {
    // The standard Windows path API exposes native paths as valid UTF-16 text;
    // unpaired native path bytes are therefore not observable here.
    value
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character == '%' || character.is_control() {
                let value = character as u32;
                if value <= u32::from(u8::MAX) {
                    format!("%{value:02X}")
                } else {
                    // This branch is defensive for non-Windows targets whose
                    // native text can contain a non-byte control code point.
                    format!("%{:02X}", value & 0xff)
                }
            } else {
                character.to_string()
            }
        })
        .collect()
}

#[cfg(unix)]
fn encode_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte < 0x80 {
            if (0x20..=0x7e).contains(&byte) && byte != b'%' {
                output.push(byte as char);
            } else {
                push_byte_escape(&mut output, byte);
            }
            index += 1;
            continue;
        }

        match std::str::from_utf8(&bytes[index..]) {
            Ok(text) => {
                output.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // valid_up_to() always ends on a UTF-8 boundary.
                    if let Some(text) = std::str::from_utf8(&bytes[index..index + valid]).ok() {
                        output.push_str(text);
                        index += valid;
                    } else {
                        push_byte_escape(&mut output, byte);
                        index += 1;
                    }
                } else {
                    push_byte_escape(&mut output, byte);
                    index += 1;
                }
            }
        }
    }
    output
}

#[cfg(unix)]
fn push_byte_escape(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

#[cfg(test)]
mod tests {
    use super::encode_path_for_output;
    use std::path::Path;

    #[test]
    fn printable_paths_are_literal_except_percent() {
        assert_eq!(
            encode_path_for_output(Path::new("a b/c%file")),
            "a b/c%25file"
        );
    }

    #[test]
    fn line_breaks_are_escaped() {
        assert_eq!(
            encode_path_for_output(Path::new("line\nreturn\r")),
            "line%0Areturn%0D"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_valid_non_ascii_and_invalid_bytes_are_distinguished() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        assert_eq!(encode_path_for_output(Path::new("café/文件")), "café/文件");
        let invalid = OsString::from_vec(b"bad\xff\xfe/name".to_vec());
        assert_eq!(
            encode_path_for_output(Path::new(&invalid)),
            "bad%FF%FE/name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn all_ascii_controls_and_del_are_escaped() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let value = OsString::from_vec(vec![b'a', 0, b'b', 0x1f, b'c', 0x7f]);
        assert_eq!(encode_path_for_output(Path::new(&value)), "a%00b%1Fc%7F");
    }
}
