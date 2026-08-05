use std::process::ExitCode;

use jj_conflict_untangler::{CliError, Command, DomainError, USAGE, parse_args};

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(command) => {
            let operation = match command {
                Command::Prepare(_) => "prepare",
                Command::Apply(_) => "apply",
            };
            eprintln!("{}", DomainError::NotImplemented { operation });
            ExitCode::from(1)
        }
        Err(CliError::HelpRequested) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
