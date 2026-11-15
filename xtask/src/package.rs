//! `cargo xtask package [--out DIR]`: a ready-to-run Windows package of ShareCraft, a folder
//! `ShareCraft-<version>-windows/` and its zip, that installs on a machine with no compiler, no
//! Git and no PostgreSQL.
//!
//! It builds `sharecraft-server` and `xtask` in release mode, builds the browser client as
//! `cargo xtask web` does, and refuses unless every editor in `editors.toml` is installed and
//! matches it. It then copies, under the package folder:
//!
//! - `bin\sharecraft-server.exe` and `bin\xtask.exe`;
//! - `web\` (without the page script tests) and `editors\<id>\` (with their `.br`/`.gz` copies);
//! - `pgsql\bin`, `pgsql\lib` and `pgsql\share` from `SHARECRAFT_PG_DIR`, or from
//!   `C:\Program Files\PostgreSQL\18`, without pgAdmin, documentation, symbols, headers,
//!   StackBuilder or the installer's own files;
//! - the contents of `packaging\windows\` (the install scripts and the guide), if it exists;
//!
//! and writes `LICENCES.html` and `VERSION`. Nothing named `.env`, `data`, `target`, `.git`,
//! `test` or `tests`, no dot file and no test script is ever copied, and the finished folder is
//! checked for them before it is zipped. The zip is made by Windows PowerShell with .NET's
//! `System.IO.Compression` (no new dependency), with `/` between the parts of each entry's name.
//!
//! The parts that choose files and write text do no building, so the tests below run them on
//! small folders of stand-in files.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, cargo, editors, run, target_dir, web, workspace_root};

/// Where the package goes when `--out` is not given.
const DEFAULT_OUT: &str = r"J:\sharecraft-package";
/// Where PostgreSQL is copied from when `SHARECRAFT_PG_DIR` is not set.
const DEFAULT_PG_DIR: &str = r"C:\Program Files\PostgreSQL\18";
/// The variable naming another PostgreSQL installation to copy.
const PG_DIR_VARIABLE: &str = "SHARECRAFT_PG_DIR";
/// The folder whose contents (scripts and guide) go into the package root.
const PACKAGING_DIR: [&str; 2] = ["packaging", "windows"];
/// Where the project's source, and its build guide, are published.
const REPOSITORY: &str = "https://github.com/ThineCraft/ShareCraft";
/// The PostgreSQL programs the install and start scripts cannot do without.
const PG_REQUIRED: [&str; 3] = ["postgres.exe", "initdb.exe", "pg_ctl.exe"];

/// Names (compared without regard to case) never copied into a package, wherever they are.
const NEVER_COPIED: [&str; 6] = [".env", "data", "target", ".git", "test", "tests"];
/// File name endings (compared without regard to case) of test files, never copied.
const TEST_SUFFIXES: [&str; 4] = [".test.mjs", ".test.js", ".tests.ps1", ".test.ps1"];
/// Folder names (compared without regard to case) left out of the PostgreSQL copy.
const PG_DROPPED_DIRS: [&str; 4] = ["doc", "include", "installer", "symbols"];
/// Name prefixes (compared without regard to case) left out of the PostgreSQL copy: pgAdmin,
/// StackBuilder and the uninstaller.
const PG_DROPPED_PREFIXES: [&str; 3] = ["pgadmin", "stackbuilder", "uninstall"];

/// Runs `cargo xtask package [--out DIR]`.
pub fn run_package(args: &[String]) -> Result<()> {
    let out = parse_args(args)?;
    let root = workspace_root()?;
    // The quick refusals come before the long build.
    let editors = editors::require_all_installed(&root)?;
    let postgres = postgres_dir()?;
    check_postgres(&postgres)?;
    let version = version_info(&root)?;

    run(cargo(&root).args([
        "build",
        "--release",
        "-p",
        "server",
        "--bin",
        "sharecraft-server",
        "-p",
        "xtask",
        "--bin",
        "xtask",
    ]))?;
    web()?;
    let release = target_dir(&root)?.join("release");

    let name = package_name(&version.name);
    let folder = out.join(&name);
    let zip = out.join(format!("{name}.zip"));
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    remove_old(&folder, &zip)?;
    let sources = Sources {
        root,
        server: release.join("sharecraft-server.exe"),
        xtask: release.join("xtask.exe"),
        postgres,
        editors,
        version,
    };
    stage(&sources, &folder)?;
    compress(&folder, &zip)?;
    let size = std::fs::metadata(&zip)
        .map_err(|e| format!("cannot read {}: {e}", zip.display()))?
        .len();
    println!("xtask: wrote {}", folder.display());
    println!(
        "xtask: wrote {} ({} MB, {size} bytes)",
        zip.display(),
        editors::megabytes(size)
    );
    Ok(())
}

/// The output folder from `[--out DIR]`.
fn parse_args(args: &[String]) -> Result<PathBuf> {
    match args {
        [] => Ok(PathBuf::from(DEFAULT_OUT)),
        [flag, dir] if flag == "--out" && !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Err("usage: cargo xtask package [--out DIR]".to_owned()),
    }
}

/// The PostgreSQL installation to copy.
fn postgres_dir() -> Result<PathBuf> {
    match std::env::var_os(PG_DIR_VARIABLE) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(PathBuf::from(DEFAULT_PG_DIR)),
    }
}

/// Refuses a PostgreSQL folder without the programs the scripts run.
fn check_postgres(dir: &Path) -> Result<()> {
    let missing: Vec<&str> = PG_REQUIRED
        .into_iter()
        .filter(|exe| !dir.join("bin").join(exe).is_file())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} is not a PostgreSQL installation (no bin\\{}); install PostgreSQL 18 there or \
             set {PG_DIR_VARIABLE} to one",
            dir.display(),
            missing.join(", bin\\")
        ))
    }
}

/// What the package is built from, for its name and its `VERSION` file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    /// The name used in the folder and the zip: the server's version and the short commit id,
    /// with `-modified` when the checkout has uncommitted changes.
    name: String,
    /// The full commit id.
    commit: String,
    branch: String,
    /// The build date, `YYYY-MM-DD`, in UTC.
    date: String,
    /// Whether tracked files had uncommitted changes.
    modified: bool,
}

fn version_info(root: &Path) -> Result<Version> {
    let manifest_path = root.join("crates").join("server").join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let crate_version = server_version(&manifest)?;
    let commit = editors::git(root, &["rev-parse", "HEAD"])?;
    let branch = editors::git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let modified =
        !editors::git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    Ok(Version {
        name: version_name(&crate_version, &commit, modified),
        commit,
        branch,
        date: editors::today(),
        modified,
    })
}

/// The `version` of the server crate's manifest.
fn server_version(manifest: &str) -> Result<String> {
    let table: toml::Table = manifest
        .parse()
        .map_err(|e| format!("crates/server/Cargo.toml is not valid TOML: {e}"))?;
    table
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "crates/server/Cargo.toml has no package version".to_owned())
}

/// `0.1.0-d0a75ec`, or `0.1.0-d0a75ec-modified`.
fn version_name(crate_version: &str, commit: &str, modified: bool) -> String {
    let short = commit.get(..7).unwrap_or(commit);
    let suffix = if modified { "-modified" } else { "" };
    format!("{crate_version}-{short}{suffix}")
}

/// `ShareCraft-<version>-windows`, the folder's name and the zip's without `.zip`.
fn package_name(version: &str) -> String {
    format!("ShareCraft-{version}-windows")
}

/// The text of the package's `VERSION` file.
fn version_file(version: &Version) -> String {
    let state = if version.modified {
        "with uncommitted changes"
    } else {
        "clean"
    };
    format!(
        "ShareCraft {}\r\ncommit: {}\r\nbranch: {}\r\nbuilt: {} (UTC)\r\nworking tree: {state}\r\n",
        version.name, version.commit, version.branch, version.date
    )
}

/// Removes the package folder and zip left by an earlier run with the same version.
fn remove_old(folder: &Path, zip: &Path) -> Result<()> {
    if folder.exists() {
        std::fs::remove_dir_all(folder)
            .map_err(|e| format!("cannot remove the old {}: {e}", folder.display()))?;
    }
    if zip.exists() {
        std::fs::remove_file(zip)
            .map_err(|e| format!("cannot remove the old {}: {e}", zip.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Choosing files

/// Lower-case names of the components of a relative path.
fn lower_components(rel: &Path) -> Vec<String> {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// Whether a path, relative to the folder being copied, must never go into a package: anything
/// named in [`NEVER_COPIED`] or inside it, dot files and dot folders, and test scripts.
fn never_copied(rel: &Path) -> bool {
    let names = lower_components(rel);
    let forbidden = names
        .iter()
        .any(|name| name.starts_with('.') || NEVER_COPIED.contains(&name.as_str()));
    let test_file = names
        .last()
        .is_some_and(|name| TEST_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)));
    forbidden || test_file
}

/// Whether a path relative to the PostgreSQL installation goes into the package's `pgsql`
/// folder. Folders are asked about before their contents; a folder refused is not entered.
///
/// Kept: `bin` (its programs and libraries only), `lib` and `share`. Left out: everything else
/// at the top (the cluster's `data`, `doc`, `include`, `pgAdmin 4`, `installer`, the
/// uninstaller), documentation and headers anywhere, symbols (`*.pdb`), StackBuilder and the
/// wxWidgets libraries only it uses, link libraries (`*.lib`, `*.a`), and anything
/// [`never_copied`].
fn keep_postgres(rel: &Path, is_dir: bool) -> bool {
    let names = lower_components(rel);
    let Some(top) = names.first() else {
        return false;
    };
    if !["bin", "lib", "share"].contains(&top.as_str()) || never_copied(rel) {
        return false;
    }
    let dropped = names.iter().any(|name| {
        PG_DROPPED_DIRS.contains(&name.as_str())
            || PG_DROPPED_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
    });
    if dropped {
        return false;
    }
    if is_dir {
        return true;
    }
    let Some(name) = names.last() else {
        return false;
    };
    let extension = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    if ["pdb", "lib", "a"].contains(&extension.as_str()) {
        return false;
    }
    if top == "bin" {
        let program = ["exe", "dll"].contains(&extension.as_str());
        let wx = name.starts_with("wx") && extension == "dll";
        return program && !wx;
    }
    true
}

/// Copies what `keep` accepts of the folder `from` to `to`. `keep` gets each path relative to
/// `base` (`from` is `base` joined with `rel`) and whether it is a folder. Returns the number of
/// files copied.
fn copy_tree(
    base: &Path,
    rel: &Path,
    to: &Path,
    keep: &dyn Fn(&Path, bool) -> bool,
) -> Result<u64> {
    let from = base.join(rel);
    let failed =
        |e: std::io::Error| format!("cannot copy {} to {}: {e}", from.display(), to.display());
    std::fs::create_dir_all(to).map_err(failed)?;
    let mut count = 0;
    for entry in std::fs::read_dir(&from).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let child = rel.join(entry.file_name());
        let is_dir = entry.file_type().map_err(failed)?.is_dir();
        if !keep(&child, is_dir) {
            continue;
        }
        let destination = to.join(entry.file_name());
        if is_dir {
            count += copy_tree(base, &child, &destination, keep)?;
        } else {
            std::fs::copy(entry.path(), &destination).map_err(failed)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Copies a folder of the project, without anything [`never_copied`].
fn copy_project_dir(from: &Path, to: &Path) -> Result<u64> {
    copy_tree(from, Path::new(""), to, &|rel, _| !never_copied(rel))
}

/// Every path under `dir`, relative to it, that [`never_copied`] refuses. Empty for a good
/// package.
fn forbidden_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(base: &Path, rel: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
        let path = base.join(rel);
        let failed = |e: std::io::Error| format!("cannot list {}: {e}", path.display());
        for entry in std::fs::read_dir(&path).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            let child = rel.join(entry.file_name());
            if never_copied(&child) {
                found.push(child);
            } else if entry.file_type().map_err(failed)?.is_dir() {
                walk(base, &child, found)?;
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(dir, Path::new(""), &mut found)?;
    Ok(found)
}

// ---------------------------------------------------------------------------------------------
// Assembling the folder

/// Everything a package is made from.
struct Sources {
    /// The workspace root: `web`, `editors`, `packaging\windows` and the licence texts.
    root: PathBuf,
    /// The release builds.
    server: PathBuf,
    xtask: PathBuf,
    /// The PostgreSQL installation to copy.
    postgres: PathBuf,
    /// The ids of the editors to copy, all checked against `editors.toml`.
    editors: Vec<String>,
    version: Version,
}

/// Fills `folder`, which must not exist yet, with the package. Refuses, leaving what it wrote,
/// if anything that must never be copied ends up in it.
fn stage(sources: &Sources, folder: &Path) -> Result<()> {
    if folder.exists() {
        return Err(format!("{} already exists", folder.display()));
    }
    let bin = folder.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("cannot create {}: {e}", bin.display()))?;
    for (exe, name) in [
        (&sources.server, "sharecraft-server.exe"),
        (&sources.xtask, "xtask.exe"),
    ] {
        std::fs::copy(exe, bin.join(name))
            .map_err(|e| format!("cannot copy {}: {e}", exe.display()))?;
    }

    let web = sources.root.join("web");
    if !web.join("pkg").is_dir() {
        return Err(format!(
            "{} is missing; run `cargo xtask web`",
            web.join("pkg").display()
        ));
    }
    copy_project_dir(&web, &folder.join("web"))?;

    for id in &sources.editors {
        let from = sources.root.join("editors").join(id);
        copy_project_dir(&from, &folder.join("editors").join(id))?;
    }

    let pgsql = folder.join("pgsql");
    for top in ["bin", "lib", "share"] {
        if keep_postgres(Path::new(top), true) {
            copy_tree(
                &sources.postgres,
                Path::new(top),
                &pgsql.join(top),
                &keep_postgres,
            )?;
        }
    }

    let packaging = PACKAGING_DIR
        .iter()
        .fold(sources.root.clone(), |path, part| path.join(part));
    if packaging.is_dir() {
        copy_project_dir(&packaging, folder)?;
    } else {
        println!(
            "xtask: {} does not exist; the package has no install scripts or guide",
            packaging.display()
        );
    }

    write(
        &folder.join("LICENCES.html"),
        &licences_html(&sources.root, &sources.postgres, &sources.editors)?,
    )?;
    write(&folder.join("VERSION"), &version_file(&sources.version))?;

    let forbidden = forbidden_entries(folder)?;
    if !forbidden.is_empty() {
        let shown: Vec<String> = forbidden.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "the package holds files that must never be copied:\n  {}",
            shown.join("\n  ")
        ));
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------------------------
// LICENCES.html

/// Text made safe to put inside HTML.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Reads a licence text; a missing file is `None`.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// The text files at the top of the PostgreSQL installation whose names hold "licen", sorted.
fn postgres_licence_files(postgres: &Path) -> Result<Vec<PathBuf>> {
    let failed = |e: std::io::Error| format!("cannot list {}: {e}", postgres.display());
    let mut found = Vec::new();
    for entry in std::fs::read_dir(postgres).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.contains("licen")
            && name.ends_with(".txt")
            && entry.file_type().map_err(failed)?.is_file()
        {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// `LICENCES.html`: ShareCraft's licence texts, PostgreSQL's (as its installation ships them),
/// where the editors' licences are, and a pointer to the build guide for the Rust crates.
fn licences_html(root: &Path, postgres: &Path, editors: &[String]) -> Result<String> {
    let mut sections = String::new();
    let pre = |title: &str, text: &str| {
        format!(
            "<h3>{}</h3>\n<pre>{}</pre>\n",
            escape_html(title),
            escape_html(text.trim_end())
        )
    };

    sections.push_str(
        "<h2>ShareCraft</h2>\n<p>ShareCraft is licensed under the MIT licence or the Apache \
         Licence 2.0, at your choice.</p>\n",
    );
    for file in ["LICENSE-MIT", "LICENSE-APACHE"] {
        let text = read_optional(&root.join(file))?
            .ok_or_else(|| format!("{} is missing", root.join(file).display()))?;
        sections.push_str(&pre(file, &text));
    }

    sections.push_str(
        "<h2>PostgreSQL</h2>\n<p>The <code>pgsql</code> folder is a copy of PostgreSQL, \
         distributed under the PostgreSQL Licence, with the libraries its Windows build uses \
         under their own licences. The texts below are the ones its installation ships.</p>\n",
    );
    let files = postgres_licence_files(postgres)?;
    if files.is_empty() {
        sections.push_str(
            "<p>The PostgreSQL installation this package was made from held no licence files; \
             the PostgreSQL Licence is at \
             <a href=\"https://www.postgresql.org/about/licence/\">postgresql.org/about/licence</a>.\
             </p>\n",
        );
    }
    for path in files {
        let text = read_optional(&path)?.unwrap_or_default();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        sections.push_str(&pre(&name, &text));
    }

    sections.push_str(
        "<h2>The editors</h2>\n<p>The editors are licensed MIT or Apache 2.0 by their \
         original authors. Each one's licence, notice and attribution files are in its \
         <code>licenses</code> folder:</p>\n<table>\n<thead><tr><th>Editor</th><th>Folder</th>\
         </tr></thead>\n<tbody>\n",
    );
    for id in editors {
        let id = escape_html(id);
        sections.push_str(&format!(
            "<tr><td>{id}</td><td><code>editors\\{id}\\licenses</code></td></tr>\n"
        ));
    }
    sections.push_str("</tbody>\n</table>\n");

    sections.push_str(&format!(
        "<h2>The Rust crates</h2>\n<p>The programs in <code>bin</code> are built from \
         ShareCraft's source and the Rust crates it depends on, none of them under a copyleft \
         licence. The crates and their licences are described in the Licences section of \
         <code>BUILDING.html</code> in the source: \
         <a href=\"{REPOSITORY}/blob/main/BUILDING.html#licences\">{REPOSITORY}</a>.</p>\n"
    ));

    Ok(format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>ShareCraft licences</title>\n<style>\n{LICENCES_CSS}</style>\n</head>\n<body>\n\
         <h1>Licences</h1>\n{sections}</body>\n</html>\n"
    ))
}

const LICENCES_CSS: &str = ":root { color-scheme: light dark; --bg: #ffffff; --fg: #1d1d1f; \
--muted: #f2f2f4; --band: #f6f6f8; --line: #d8d8dc; --link: #0b57d0; }
@media (prefers-color-scheme: dark) { :root { --bg: #17171a; --fg: #e8e8ea; --muted: #232327; \
--band: #1e1e22; --line: #3a3a40; --link: #8ab4f8; } }
body { background: var(--bg); color: var(--fg); font: 16px/1.5 system-ui, sans-serif; \
max-width: 60rem; margin: 0 auto; padding: 1rem; }
a { color: var(--link); }
pre { background: var(--muted); border: 1px solid var(--line); padding: 0.75rem; \
overflow-x: auto; white-space: pre-wrap; font-size: 0.85rem; }
table { border-collapse: collapse; }
th, td { border: 1px solid var(--line); padding: 0.3rem 0.6rem; text-align: left; }
tbody tr:nth-child(even) { background: var(--band); }
";

// ---------------------------------------------------------------------------------------------
// The zip

/// The Windows PowerShell script that zips the folder named by `SHARECRAFT_PACKAGE_DIR` into
/// `SHARECRAFT_PACKAGE_ZIP`. `Compress-Archive` and `ZipFile.CreateFromDirectory` in Windows
/// PowerShell 5.1 name entries with `\`, which the zip format does not allow and some tools
/// refuse; each entry is added by hand instead, named with `/`.
const ZIP_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$dir = (Get-Item -LiteralPath $env:SHARECRAFT_PACKAGE_DIR).FullName.TrimEnd('\\')
$top = Split-Path -Leaf $dir
$zip = [System.IO.Compression.ZipFile]::Open($env:SHARECRAFT_PACKAGE_ZIP, 'Create')
try {
  foreach ($file in Get-ChildItem -LiteralPath $dir -Recurse -File -Force) {
    $name = $top + '/' + $file.FullName.Substring($dir.Length + 1).Replace('\\', '/')
    [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file.FullName, $name, 'Optimal')
  }
} finally { $zip.Dispose() }
";

/// Zips `folder`, as the top folder of the archive, into `zip`. The paths reach PowerShell
/// through the environment, not its command line, so spaces and other characters in them need
/// no quoting.
fn compress(folder: &Path, zip: &Path) -> Result<()> {
    run(Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", ZIP_SCRIPT])
        .env("SHARECRAFT_PACKAGE_DIR", folder)
        .env("SHARECRAFT_PACKAGE_ZIP", zip))?;
    if zip.is_file() {
        Ok(())
    } else {
        Err(format!("PowerShell did not write {}", zip.display()))
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-package-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(base: &Path, rel: &str) {
        let path = base.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, rel).unwrap();
    }

    /// Every file under `dir`, relative to it, with `/` separators, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    let rel = path.strip_prefix(base).unwrap();
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn secrets_data_build_output_and_tests_are_never_copied() {
        for refused in [
            ".env",
            "web/.env",
            "data/sharecraft.db",
            "Data",
            "target/release/x.exe",
            ".git/config",
            "web/pkg/.gitignore",
            "tests/install.Tests.ps1",
            "scripts/install.Tests.ps1",
            "test/fixture.txt",
            "web/common.test.mjs",
            "lib/pgxs/src/test/perl/x.pm",
        ] {
            assert!(never_copied(Path::new(refused)), "{refused}");
        }
        for kept in [
            "web/index.html",
            "web/pkg/web_client_bg.wasm",
            "editors/gridcraft/index.html.br",
            "scripts/install.ps1",
            "Install ShareCraft.cmd",
            "README.html",
            "share/timezone/Europe/London",
            "latest/database.js",
        ] {
            assert!(!never_copied(Path::new(kept)), "{kept}");
        }
    }

    #[test]
    fn postgres_keeps_its_programs_libraries_and_shared_files() {
        for kept in [
            "bin/postgres.exe",
            "bin/initdb.exe",
            "bin/pg_ctl.exe",
            "bin/libpq.dll",
            "bin/icuuc77.dll",
            "bin/zlib1.dll",
            "lib/plpgsql.dll",
            "lib/pgxs/config/install-sh",
            "share/postgres.bki",
            "share/timezone/UTC",
            "share/extension/plpgsql.control",
            "share/locale/de/LC_MESSAGES/postgres-18.mo",
        ] {
            assert!(keep_postgres(Path::new(kept), false), "{kept}");
        }
        for dir in ["bin", "lib", "share", "share/timezone", "lib/pgxs"] {
            assert!(keep_postgres(Path::new(dir), true), "{dir}");
        }
    }

    #[test]
    fn postgres_drops_tools_documents_symbols_and_installer_files() {
        for dropped in [
            "data/postgresql.conf",
            "pgAdmin 4/runtime/pgAdmin4.exe",
            "bin/pgAdmin4.exe",
            "doc/postgresql/html/index.html",
            "share/doc/extension/README",
            "include/libpq-fe.h",
            "installer/vcredist_x64.exe",
            "bin/stackbuilder.exe",
            "bin/wxbase3211u_vc_x64_custom.dll",
            "bin/postgres.pdb",
            "lib/plpgsql.pdb",
            "bin/libcurl.lib",
            "lib/libpq.lib",
            "lib/libpgcommon.a",
            "lib/pgxs/src/test/perl/PostgreSQL/Test/Utils.pm",
            "symbols/postgres.pdb",
            "uninstall-postgresql.exe",
            "uninstall-postgresql.dat",
            "installation_summary.log",
            "pg_env.bat",
            "server_license.txt",
            "scripts/runpsql.bat",
            "bin/README.txt",
        ] {
            assert!(!keep_postgres(Path::new(dropped), false), "{dropped}");
        }
        for dir in [
            "data",
            "doc",
            "include",
            "pgAdmin 4",
            "installer",
            "share/doc",
        ] {
            assert!(!keep_postgres(Path::new(dir), true), "{dir}");
        }
    }

    /// A stand-in workspace, PostgreSQL installation and release builds, with files that must
    /// not reach the package mixed in.
    fn fake_sources(base: &Path, with_packaging: bool) -> Sources {
        let root = base.join("repo");
        for rel in [
            ".env",
            "data/library/x.txt",
            "target/release/sharecraft-server.exe",
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "web/index.html",
            "web/common.js",
            "web/common.test.mjs",
            "web/.env",
            "web/pkg/web_client.js",
            "web/pkg/web_client_bg.wasm",
            "web/pkg/.gitignore",
            "editors/gridcraft/index.html",
            "editors/gridcraft/index.html.br",
            "editors/gridcraft/index.html.gz",
            "editors/gridcraft/editor.json",
            "editors/gridcraft/licenses/LICENSE",
            "editors/gridcraft/.trunk-lock",
            "editors/notlisted/index.html",
        ] {
            touch(&root, rel);
        }
        if with_packaging {
            for rel in [
                "packaging/windows/Install ShareCraft.cmd",
                "packaging/windows/README.html",
                "packaging/windows/scripts/install.ps1",
                "packaging/windows/scripts/start.ps1",
                "packaging/windows/tests/install.Tests.ps1",
                "packaging/windows/.env",
            ] {
                touch(&root, rel);
            }
        }
        let postgres = base.join("PostgreSQL 18");
        for rel in [
            "bin/postgres.exe",
            "bin/initdb.exe",
            "bin/pg_ctl.exe",
            "bin/libpq.dll",
            "bin/stackbuilder.exe",
            "bin/postgres.pdb",
            "lib/plpgsql.dll",
            "lib/libpq.lib",
            "share/postgres.bki",
            "share/doc/extension/README",
            "doc/index.html",
            "include/libpq-fe.h",
            "data/postgresql.conf",
            "pgAdmin 4/pgAdmin4.exe",
            "server_license.txt",
            "uninstall-postgresql.exe",
        ] {
            touch(&postgres, rel);
        }
        touch(base, "release/sharecraft-server.exe");
        touch(base, "release/xtask.exe");
        Sources {
            root,
            server: base.join("release").join("sharecraft-server.exe"),
            xtask: base.join("release").join("xtask.exe"),
            postgres,
            editors: vec!["gridcraft".to_owned()],
            version: Version {
                name: "0.1.0-d0a75ec".to_owned(),
                commit: "d0a75ec0000000000000000000000000000000000".to_owned(),
                branch: "windows-package".to_owned(),
                date: "2026-11-15".to_owned(),
                modified: false,
            },
        }
    }

    #[test]
    fn a_package_has_the_layout_and_nothing_it_must_not() {
        let base = scratch("stage");
        let sources = fake_sources(&base, true);
        let folder = base.join("out").join(package_name(&sources.version.name));
        stage(&sources, &folder).unwrap();
        assert_eq!(
            folder.file_name().unwrap().to_string_lossy(),
            "ShareCraft-0.1.0-d0a75ec-windows"
        );
        assert_eq!(
            listing(&folder),
            [
                "Install ShareCraft.cmd",
                "LICENCES.html",
                "README.html",
                "VERSION",
                "bin/sharecraft-server.exe",
                "bin/xtask.exe",
                "editors/gridcraft/editor.json",
                "editors/gridcraft/index.html",
                "editors/gridcraft/index.html.br",
                "editors/gridcraft/index.html.gz",
                "editors/gridcraft/licenses/LICENSE",
                "pgsql/bin/initdb.exe",
                "pgsql/bin/libpq.dll",
                "pgsql/bin/pg_ctl.exe",
                "pgsql/bin/postgres.exe",
                "pgsql/lib/plpgsql.dll",
                "pgsql/share/postgres.bki",
                "scripts/install.ps1",
                "scripts/start.ps1",
                "web/common.js",
                "web/index.html",
                "web/pkg/web_client.js",
                "web/pkg/web_client_bg.wasm",
            ]
        );
        let version = std::fs::read_to_string(folder.join("VERSION")).unwrap();
        assert!(version.contains("commit: d0a75ec0000"), "{version}");
        assert!(version.contains("branch: windows-package"), "{version}");
        assert!(version.contains("built: 2026-11-15 (UTC)"), "{version}");
        let licences = std::fs::read_to_string(folder.join("LICENCES.html")).unwrap();
        for expected in [
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "server_license.txt",
            "editors\\gridcraft\\licenses",
            "BUILDING.html#licences",
        ] {
            assert!(licences.contains(expected), "{expected}");
        }
        // A second run into the same folder is refused; the command removes it first.
        assert!(stage(&sources, &folder).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_package_without_the_packaging_folder_still_builds() {
        let base = scratch("stage-bare");
        let sources = fake_sources(&base, false);
        let folder = base.join("out").join("p");
        stage(&sources, &folder).unwrap();
        let files = listing(&folder);
        assert!(files.contains(&"bin/sharecraft-server.exe".to_owned()));
        assert!(!files.iter().any(|f| f.starts_with("scripts/")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_package_without_the_browser_client_is_refused() {
        let base = scratch("stage-nopkg");
        let sources = fake_sources(&base, false);
        std::fs::remove_dir_all(sources.root.join("web").join("pkg")).unwrap();
        let error = stage(&sources, &base.join("out").join("p")).unwrap_err();
        assert!(error.contains("cargo xtask web"), "{error}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn forbidden_entries_are_found_anywhere_in_a_folder() {
        let base = scratch("forbidden");
        touch(&base, "web/index.html");
        touch(&base, "web/.env");
        touch(&base, "editors/x/data/a.txt");
        let found: Vec<String> = forbidden_entries(&base)
            .unwrap()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        let mut found = found;
        found.sort();
        assert_eq!(found, ["editors/x/data", "web/.env"]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_postgres_folder_without_its_programs_is_refused() {
        let base = scratch("pg-missing");
        touch(&base, "bin/postgres.exe");
        let error = check_postgres(&base).unwrap_err();
        assert!(
            error.contains("initdb.exe") && error.contains("pg_ctl.exe"),
            "{error}"
        );
        assert!(error.contains(PG_DIR_VARIABLE), "{error}");
        touch(&base, "bin/initdb.exe");
        touch(&base, "bin/pg_ctl.exe");
        check_postgres(&base).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn versions_name_the_commit() {
        assert_eq!(
            version_name("0.1.0", "d0a75ec1234567", false),
            "0.1.0-d0a75ec"
        );
        assert_eq!(
            version_name("0.1.0", "d0a75ec1234567", true),
            "0.1.0-d0a75ec-modified"
        );
        assert_eq!(
            server_version("[package]\nname = \"server\"\nversion = \"0.1.0\"\n").unwrap(),
            "0.1.0"
        );
    }

    #[test]
    fn arguments_choose_the_output_folder() {
        assert_eq!(parse_args(&[]).unwrap(), PathBuf::from(DEFAULT_OUT));
        assert_eq!(
            parse_args(&["--out".to_owned(), "D:\\p".to_owned()]).unwrap(),
            PathBuf::from("D:\\p")
        );
        assert!(parse_args(&["--out".to_owned()]).is_err());
        assert!(parse_args(&["--zip".to_owned(), "x".to_owned()]).is_err());
    }

    /// The zip holds the folder as its top entry, every name with `/`, from a path with a space
    /// and a non-ASCII letter.
    #[cfg(windows)]
    #[test]
    fn the_zip_names_entries_with_forward_slashes() {
        let base = scratch("zip");
        let folder = base.join("Zo\u{eb} pkg");
        touch(&folder, "bin/a.exe");
        touch(&folder, "Install ShareCraft.cmd");
        let zip = base.join("out.zip");
        compress(&folder, &zip).unwrap();
        let out = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Add-Type -AssemblyName System.IO.Compression.FileSystem; \
                 [Console]::OutputEncoding = [Text.Encoding]::UTF8; \
                 $z = [System.IO.Compression.ZipFile]::OpenRead($env:ZIP_TO_LIST); \
                 $z.Entries | ForEach-Object { $_.FullName }; $z.Dispose()",
            ])
            .env("ZIP_TO_LIST", &zip)
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let mut names: Vec<&str> = text.lines().collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "Zo\u{eb} pkg/Install ShareCraft.cmd",
                "Zo\u{eb} pkg/bin/a.exe"
            ]
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn licence_texts_are_escaped() {
        assert_eq!(escape_html("<a & \"b\">"), "&lt;a &amp; &quot;b&quot;&gt;");
    }
}
