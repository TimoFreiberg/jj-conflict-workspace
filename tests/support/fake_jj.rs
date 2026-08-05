use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const SNAPSHOT_ENV: &str = "JCW_TEST_FAKE_JJ_SNAPSHOT";
const ARGV_LOG_ENV: &str = "JCW_TEST_FAKE_JJ_ARGV_LOG";
const STDERR_ENV: &str = "JCW_TEST_FAKE_JJ_STDERR_FILE";
const EXIT_ENV: &str = "JCW_TEST_FAKE_JJ_EXIT";
const MUTATE_ENV: &str = "JCW_TEST_FAKE_JJ_MUTATE_SOURCE";
const REPLACE_ENV: &str = "JCW_TEST_FAKE_JJ_REPLACE_SOURCE";

fn fail(message: &str) -> ! {
    eprintln!("fake-jj: {message}");
    std::process::exit(70);
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
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
        return value.encode_wide().flat_map(u16::to_le_bytes).collect();
    }
    #[cfg(not(any(unix, windows)))]
    value.to_string_lossy().as_bytes().to_vec()
}

fn record_arguments(path: &Path, arguments: &[OsString]) -> io::Result<()> {
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    for argument in arguments {
        let payload = native_bytes(argument.as_os_str());
        let length = u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument is too long"))?;
        output.write_all(&length.to_be_bytes())?;
        output.write_all(&payload)?;
    }
    output.sync_all()
}

fn replace_identically(source: &Path) -> io::Result<()> {
    let bytes = fs::read(source)?;
    let parent = source
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"))?;
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no name"))?
        .to_owned();
    let temporary = parent.join(format!(".jcw-fake-replacement-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    {
        // Windows does not replace an existing file with std::fs::rename.
        // Removing the destination still gives the test a fresh file identity.
        fs::remove_file(source)?;
    }
    let result = fs::rename(&temporary, parent.join(name));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn main() {
    // Validate action and exit settings before touching any env-specified path.
    let mutate = env::var_os(MUTATE_ENV);
    let replace = env::var_os(REPLACE_ENV);
    if mutate.is_some() && replace.is_some() {
        fail("mutate and replace actions cannot both be set");
    }
    if let Some(action) = mutate.as_deref() {
        if action != OsStr::new("mutate-bytes") {
            fail("invalid source mutation action");
        }
    }
    if let Some(action) = replace.as_deref() {
        if action != OsStr::new("replace-identical") {
            fail("invalid source replacement action");
        }
    }
    let exit_code = match env::var_os(EXIT_ENV) {
        None => 0,
        Some(value) => match value.to_str().and_then(|text| text.parse::<u32>().ok()) {
            Some(value) if value <= i32::MAX as u32 => value as i32,
            _ => fail("invalid fake JJ exit code"),
        },
    };

    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    if let Some(path) = env_path(ARGV_LOG_ENV) {
        if let Err(error) = record_arguments(&path, &arguments) {
            eprintln!("fake-jj: could not record arguments: {error}");
            std::process::exit(71);
        }
    }

    if mutate.is_some() || replace.is_some() {
        let relative = match arguments.last() {
            Some(path) => path,
            None => fail("missing production source path argument"),
        };
        let current_dir = match env::current_dir() {
            Ok(path) => path,
            Err(_) => fail("could not determine current directory"),
        };
        let source = current_dir.join(relative);
        let result = if mutate.is_some() {
            fs::write(&source, b"fake-jj mutated source bytes\n")
        } else {
            replace_identically(&source)
        };
        if result.is_err() {
            fail("could not mutate source");
        }
    }

    if let Some(path) = env_path(SNAPSHOT_ENV) {
        let mut bytes = Vec::new();
        match fs::File::open(path).and_then(|mut file| file.read_to_end(&mut bytes)) {
            Ok(_) => {
                if let Err(_) = io::stdout().write_all(&bytes) {
                    std::process::exit(72);
                }
            }
            Err(_) => fail("could not read snapshot output"),
        }
    }
    if let Some(path) = env_path(STDERR_ENV) {
        let mut bytes = Vec::new();
        match fs::File::open(path).and_then(|mut file| file.read_to_end(&mut bytes)) {
            Ok(_) => {
                if let Err(_) = io::stderr().write_all(&bytes) {
                    std::process::exit(73);
                }
            }
            Err(_) => fail("could not read stderr output"),
        }
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}
