#![forbid(unsafe_code)]
//! `sharecraft-server`: reads its settings from the environment (and `.env`, if present) and
//! serves until stopped.
//!
//! `sharecraft-server add-user --email <email> --name <name> [--admin]` creates an account
//! instead, and `sharecraft-server rebuild` rebuilds the revisions recorded in the database
//! from Git; see [`server::cli`].

use std::process::ExitCode;

use server::{Config, cli, serve};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(message) = load_env() {
        eprintln!("sharecraft-server: {message}");
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        None => run_server().await,
        Some((command, rest)) if command == "add-user" => match cli::add_user(rest).await {
            Ok(done) => {
                println!("{done}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("sharecraft-server: {message}");
                ExitCode::FAILURE
            }
        },
        Some((command, rest)) if command == "rebuild" => match cli::rebuild(rest).await {
            Ok(report) => {
                println!("Rebuilt revisions from Git: {report}");
                for (doc, error) in &report.failed {
                    eprintln!("sharecraft-server: document {} failed: {error}", doc.0);
                }
                if report.failed.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(message) => {
                eprintln!("sharecraft-server: {message}");
                ExitCode::FAILURE
            }
        },
        Some((other, _)) => {
            eprintln!("sharecraft-server: unknown command {other:?}");
            eprintln!(
                "usage: sharecraft-server [add-user --email <email> --name <name> [--admin] \
                 | rebuild]"
            );
            ExitCode::FAILURE
        }
    }
}

/// Loads `.env` from the current directory or a parent, if there is one. Variables already set
/// win.
fn load_env() -> Result<(), String> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(e) if e.not_found() => Ok(()),
        // Never show the error itself: for a malformed line it prints the line, and so a secret.
        Err(dotenvy::Error::LineParse(_, index)) => {
            Err(format!("cannot read .env: line {index} is malformed"))
        }
        Err(dotenvy::Error::Io(e)) => Err(format!("cannot read .env: {}", e.kind())),
        Err(_) => Err("cannot read .env".to_owned()),
    }
}

async fn run_server() -> ExitCode {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("sharecraft-server: {err}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match TcpListener::bind(config.addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("sharecraft-server: cannot listen on {}: {err}", config.addr);
            return ExitCode::FAILURE;
        }
    };
    let addr = listener.local_addr().unwrap_or(config.addr);
    println!("ShareCraft server listening on http://{addr}");
    println!("WARNING: development mode, local connections only");
    match serve(config, listener).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sharecraft-server: {err}");
            ExitCode::FAILURE
        }
    }
}
