//! `bothy`: command-line tool for Bothy databases.

mod cli;
mod commands;
mod config;
mod fail;
mod oauth;
mod tokens;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Cli::parse();
    let paths = match config::Paths::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bothy: {e}");
            return e.exit_code();
        }
    };
    let ctx = commands::Ctx {
        paths,
        json: args.json,
        db: args.db,
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("bothy: cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(commands::run(ctx, args.command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bothy: {e}");
            e.exit_code()
        }
    }
}
