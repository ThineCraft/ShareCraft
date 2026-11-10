#![forbid(unsafe_code)]
//! `sharecraft-server`: reads its settings from the environment and serves until stopped.

use std::process::ExitCode;

use server::{Config, serve};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> ExitCode {
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
    println!("WARNING: development mode, no sign-in, local connections only");
    match serve(config, listener).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sharecraft-server: {err}");
            ExitCode::FAILURE
        }
    }
}
