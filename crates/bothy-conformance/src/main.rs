//! `bothy-conformance generate <dir>` writes the vectors; `check <dir>` verifies them.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match (args.first().map(String::as_str), args.get(1)) {
        (Some("generate"), Some(dir)) => match bothy_conformance::write_all(Path::new(dir)) {
            Ok(n) => {
                println!("wrote {n} vector files to {dir}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("bothy-conformance: {e}");
                ExitCode::FAILURE
            }
        },
        (Some("check"), Some(dir)) => match bothy_conformance::check::check_dir(Path::new(dir)) {
            Ok(n) => {
                println!("{n} cases passed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("bothy-conformance: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: bothy-conformance (generate | check) <vectors-dir>");
            ExitCode::from(2)
        }
    }
}
