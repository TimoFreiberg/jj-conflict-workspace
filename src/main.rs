use std::process::ExitCode;

use jj_conflict_workspace::{CliError, Command, DomainError, USAGE, parse_args};

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Prepare(options)) => match jj_conflict_workspace::prepare(&options) {
            Ok(workspace) => {
                println!("{}", workspace.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        },
        Ok(Command::Apply(_)) => {
            eprintln!("{}", DomainError::NotImplemented { operation: "apply" });
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
