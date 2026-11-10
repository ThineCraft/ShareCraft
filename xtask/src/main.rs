//! Project tasks, run as `cargo xtask <task>`:
//!
//! - `web` builds the browser client for WebAssembly and writes the page's package to `web/pkg`;
//! - `check` loads `.env`, then runs the licence check, formatting, lints, tests and the
//!   WebAssembly builds;
//! - `test` loads `.env`, then runs `cargo test` with the arguments given.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const WASM_TARGET: &str = "wasm32-unknown-unknown";
/// Crates that must keep compiling for the browser.
const WASM_CRATES: [&str; 4] = ["collab-core", "collab-text", "protocol", "web-client"];

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("web") => web(),
        Some("check") => check(),
        Some("test") => test(std::env::args_os().skip(2)),
        _ => Err("usage: cargo xtask <web|check|test>".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Builds `web-client` in release mode and generates its JavaScript bindings in `web/pkg`.
fn web() -> Result<()> {
    let root = workspace_root()?;
    let lock = std::fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|e| format!("cannot read Cargo.lock: {e}"))?;
    let version =
        locked_version(&lock, "wasm-bindgen").ok_or("Cargo.lock does not list wasm-bindgen")?;
    require_wasm_bindgen(version)?;

    run(cargo(&root).args([
        "build",
        "-p",
        "web-client",
        "--target",
        WASM_TARGET,
        "--release",
    ]))?;
    let wasm = target_dir(&root)?
        .join(WASM_TARGET)
        .join("release")
        .join("web_client.wasm");
    let out_dir = root.join("web").join("pkg");
    run(Command::new("wasm-bindgen")
        .arg("--target")
        .arg("web")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg(&wasm))?;
    println!("xtask: wrote {}", out_dir.display());
    Ok(())
}

/// Runs `cargo test` with `args`, after loading `.env` so database tests find their URL.
fn test(args: impl Iterator<Item = OsString>) -> Result<()> {
    let root = workspace_root()?;
    load_env(&root)?;
    run(cargo(&root).arg("test").args(args))
}

/// Loads `.env` from the workspace root into this process, and so into the commands it runs.
/// Variables already set win. A missing file is fine; the variables may come from elsewhere.
fn load_env(root: &Path) -> Result<()> {
    match dotenvy::from_path(root.join(".env")) {
        Ok(()) => Ok(()),
        Err(e) if e.not_found() => Ok(()),
        // The message names the line, never the value.
        Err(e) => Err(format!("cannot read .env: {e}")),
    }
}

/// Runs every check the project requires before a commit.
fn check() -> Result<()> {
    let root = workspace_root()?;
    load_env(&root)?;
    check_licences(&root)?;
    run(cargo(&root).args(["fmt", "--all", "--", "--check"]))?;
    run(cargo(&root).args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))?;
    run(cargo(&root).args(["test", "--workspace"]))?;
    // One build per crate, so features enabled by another crate cannot hide a problem.
    for krate in WASM_CRATES {
        run(cargo(&root).args(["build", "-p", krate, "--target", WASM_TARGET]))?;
    }
    println!("xtask: all checks passed");
    Ok(())
}

/// Fails naming every package outside this workspace whose licence is not allowed.
fn check_licences(root: &Path) -> Result<()> {
    let out = cargo(root)
        .args(["metadata", "--format-version", "1"])
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("cannot read cargo metadata: {e}"))?;
    let refused = refused_packages(&metadata);
    if refused.is_empty() {
        println!("xtask: licences are allowed");
        Ok(())
    } else {
        Err(format!(
            "these packages have a licence that is not allowed (no GPL, LGPL, AGPL, MPL, EUPL,              CDDL or SSPL):
  {}",
            refused.join("
  ")
        ))
    }
}

/// `name version (licence)` for each non-workspace package in `cargo metadata` output whose
/// licence is refused.
fn refused_packages(metadata: &serde_json::Value) -> Vec<String> {
    let members: Vec<&str> = metadata
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .map(|ids| ids.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let text = |package: &serde_json::Value, key: &str| {
        package
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    packages
        .iter()
        .filter(|package| !members.contains(&text(package, "id").as_str()))
        .filter_map(|package| {
            let licence = text(package, "license");
            (!licence_allowed(&licence)).then(|| {
                let shown = if licence.is_empty() {
                    "no licence expression"
                } else {
                    &licence
                };
                format!(
                    "{} {} ({shown})",
                    text(package, "name"),
                    text(package, "version")
                )
            })
        })
        .collect()
}

/// Licence families this project does not accept in dependencies.
const COPYLEFT: [&str; 7] = ["GPL", "LGPL", "AGPL", "MPL", "EUPL", "CDDL", "SSPL"];

/// Whether an SPDX licence expression offers a way to use the package without copyleft: true if
/// some `OR` alternative names none of the licences in [`COPYLEFT`]. An empty or unreadable
/// expression is refused.
fn licence_allowed(expr: &str) -> bool {
    // Old manifests write `MIT/Apache-2.0` for `MIT OR Apache-2.0`.
    let spaced = expr
        .replace('/', " OR ")
        .replace('(', " ( ")
        .replace(')', " ) ");
    let mut tokens = spaced.split_whitespace().peekable();
    let allowed = parse_or(&mut tokens);
    // Anything left over means the expression was not understood.
    allowed.is_some_and(|allowed| allowed && tokens.next().is_none())
}

type Tokens<'a> = std::iter::Peekable<std::str::SplitWhitespace<'a>>;

fn parse_or(tokens: &mut Tokens<'_>) -> Option<bool> {
    let mut allowed = parse_and(tokens)?;
    while tokens.next_if(|t| t.eq_ignore_ascii_case("OR")).is_some() {
        allowed |= parse_and(tokens)?;
    }
    Some(allowed)
}

fn parse_and(tokens: &mut Tokens<'_>) -> Option<bool> {
    let mut allowed = parse_term(tokens)?;
    while tokens.next_if(|t| t.eq_ignore_ascii_case("AND")).is_some() {
        allowed &= parse_term(tokens)?;
    }
    Some(allowed)
}

fn parse_term(tokens: &mut Tokens<'_>) -> Option<bool> {
    let token = tokens.next()?;
    if token == "(" {
        let allowed = parse_or(tokens)?;
        return (tokens.next()? == ")").then_some(allowed);
    }
    if token == ")" || ["AND", "OR", "WITH"].contains(&token.to_ascii_uppercase().as_str()) {
        return None;
    }
    // `GPL-2.0-only WITH Classpath-exception-2.0` is still the GPL.
    if tokens.next_if(|t| t.eq_ignore_ascii_case("WITH")).is_some() {
        tokens.next()?;
    }
    let id = token.to_ascii_uppercase();
    Some(!COPYLEFT.iter().any(|family| id.starts_with(family)))
}

/// Fails with the install command unless `wasm-bindgen` at exactly `version` is on the path.
fn require_wasm_bindgen(version: &str) -> Result<()> {
    let install = format!("cargo install wasm-bindgen-cli --version {version} --locked");
    let output = Command::new("wasm-bindgen").arg("--version").output();
    let reported = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => return Err(format!("wasm-bindgen is not installed; run:\n  {install}")),
    };
    // Prints "wasm-bindgen 0.2.130"; the CLI must match the library exactly.
    if reported.split_whitespace().nth(1) != Some(version) {
        return Err(format!(
            "wasm-bindgen reports {:?} but Cargo.lock has {version}; run:\n  {install}",
            reported.trim()
        ));
    }
    Ok(())
}

/// The version of package `name` in the text of a `Cargo.lock`.
fn locked_version<'a>(lock: &'a str, name: &str) -> Option<&'a str> {
    let wanted = format!("name = \"{name}\"");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() == wanted {
            return lines
                .next()?
                .trim()
                .strip_prefix("version = \"")?
                .strip_suffix('"');
        }
    }
    None
}

/// The directory holding this workspace's `Cargo.toml`.
fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot find the workspace root".to_owned())
}

/// Where Cargo puts build output, honouring `CARGO_TARGET_DIR` and Cargo configuration.
fn target_dir(root: &Path) -> Result<PathBuf> {
    let out = cargo(root)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("cannot read cargo metadata: {e}"))?;
    metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata has no target_directory".to_owned())
}

/// A `cargo` command run from the workspace root, using the same Cargo that runs this task.
fn cargo(root: &Path) -> Command {
    let program = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(program);
    command.current_dir(root);
    command
}

fn run(command: &mut Command) -> Result<()> {
    let shown = format!("{command:?}");
    println!("xtask: running {shown}");
    let status = command
        .status()
        .map_err(|e| format!("cannot start {shown}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{shown} failed ({status})"))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn licence_expressions_are_judged() {
        assert!(licence_allowed("MIT OR Apache-2.0"));
        assert!(!licence_allowed("MPL-2.0"));
        assert!(licence_allowed("MPL-2.0 OR MIT"));
        assert!(licence_allowed("(MIT OR Apache-2.0) AND Unicode-3.0"));
        assert!(!licence_allowed("MIT AND LGPL-2.1-or-later"));
        assert!(!licence_allowed(""));
    }

    #[test]
    fn locked_version_reads_the_named_package() {
        let lock = "[[package]]\nname = \"wasm-bindgen-macro\"\nversion = \"9.9.9\"\n\n\
                    [[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.130\"\nsource = \"x\"\n";
        assert_eq!(locked_version(lock, "wasm-bindgen"), Some("0.2.130"));
        assert_eq!(locked_version(lock, "uuid"), None);
    }
}
