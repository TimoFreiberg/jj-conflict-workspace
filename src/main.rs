use std::process::ExitCode;

use jj_conflict_workspace::{
    ApplyReport, CliError, Command, USAGE, encode_path_for_output, parse_args,
};

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Prepare(options)) => match jj_conflict_workspace::prepare(&options) {
            Ok(workspace) => {
                println!("{}", encode_path_for_output(&workspace));
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        },
        Ok(Command::Apply(options)) => match jj_conflict_workspace::apply::run(&options) {
            Ok(report) => {
                print_apply_report(&report);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        },
        Err(CliError::HelpRequested) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn print_apply_report(report: &ApplyReport) {
    if !report.dry_run {
        println!(
            "Applied {} change(s) to `{}` ({} bytes -> {} bytes).",
            report.changed_hunks,
            encode_path_for_output(&report.source_path),
            report.old_bytes,
            report.new_bytes
        );
    } else {
        println!(
            "Proposed changes for source `{}`:",
            encode_path_for_output(&report.source_path)
        );
        print!("{}", report.diff);
        println!("No files were modified (dry-run).");
    }
}
