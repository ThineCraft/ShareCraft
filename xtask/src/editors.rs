//! `cargo xtask editors <list|build|verify>`: the editors (GridCraft, WordCraft, PdfCraft,
//! SolidCraft) are built from their fork repositories, which sit beside this one, and installed
//! into `./editors/<id>/`, where the server serves them from.
//!
//! `editors.toml` at the workspace root records what each editor is built against. The parts that
//! read text (the page's inline scripts and styles, the links, `editors.toml`, the manifest) do no
//! I/O, so the tests below call them directly; the rest runs `git` and `trunk`.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use sha2::{Digest, Sha256};

use crate::{Result, run, workspace_root};

/// The file that records what each editor is built against.
const EDITORS_TOML: &str = "editors.toml";
/// The name of an installed editor's manifest.
const MANIFEST: &str = "editor.json";
/// Name prefixes (compared without regard to case) of the licence and attribution files in a
/// fork's root that are copied into its build.
const LICENCE_PREFIXES: [&str; 5] = ["licen", "notice", "attribution", "copying", "third-party"];
/// How to install the build tool.
const TRUNK_INSTALL: &str = "cargo install trunk --locked";

/// Runs `cargo xtask editors <list|build|verify>`.
pub fn dispatch(args: &[String]) -> Result<()> {
    let root = workspace_root()?;
    match args {
        [action] if action == "list" => list(&root),
        [action, id] if action == "build" => build(&root, id),
        [action] if action == "verify" => verify_installed(&root),
        _ => Err("usage: cargo xtask editors <list | build <id> | verify>".to_owned()),
    }
}

// ---------------------------------------------------------------------------------------------
// editors.toml

/// One table of `editors.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditorSpec {
    id: String,
    upstream: String,
    base: String,
    fork: String,
    branch: String,
    /// The fork commit last built and checked; empty until recorded.
    commit: String,
    /// The editor's browser app, relative to the checkout.
    app: String,
    /// The fork's checkout, relative to the workspace root.
    checkout: String,
}

/// The editors in the text of `editors.toml`, by id. Every table needs every key, as a string.
fn parse_editors(text: &str) -> Result<Vec<EditorSpec>> {
    let table: toml::Table = text
        .parse()
        .map_err(|e| format!("{EDITORS_TOML} is not valid TOML: {e}"))?;
    let mut editors = Vec::new();
    for (id, value) in &table {
        let entry = value
            .as_table()
            .ok_or_else(|| format!("{EDITORS_TOML}: [{id}] is not a table"))?;
        let field = |key: &str| -> Result<String> {
            entry
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{EDITORS_TOML}: [{id}] needs a string `{key}`"))
        };
        editors.push(EditorSpec {
            id: id.clone(),
            upstream: field("upstream")?,
            base: field("base")?,
            fork: field("fork")?,
            branch: field("branch")?,
            commit: field("commit")?,
            app: field("app")?,
            checkout: field("checkout")?,
        });
    }
    Ok(editors)
}

fn read_editors(root: &Path) -> Result<Vec<EditorSpec>> {
    let path = root.join(EDITORS_TOML);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_editors(&text)
}

// ---------------------------------------------------------------------------------------------
// The page: inline scripts and styles, links

/// A start tag of `index.html`, with the text inside it if it is a `<script>` or `<style>`.
#[derive(Debug, PartialEq, Eq)]
struct Element {
    /// The tag name in lower case.
    name: String,
    attributes: Vec<(String, String)>,
    /// What lies between `<script …>` and `</script>`, or the same for `<style>`; `None` for
    /// other elements.
    body: Option<String>,
}

impl Element {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// The index of the first `needle` in `bytes` at or after `from`.
fn find_bytes(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let tail = bytes.get(from..)?;
    tail.windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// The start tags of `html`, in order. Comments are skipped, and the text of a script or a style
/// is not read as markup. Fails on a comment, a tag, a script or a style that is not closed.
fn scan(html: &str) -> Result<Vec<Element>> {
    let bytes = html.as_bytes();
    let mut elements = Vec::new();
    let mut pos = 0;
    while let Some(open) = find_bytes(bytes, pos, b"<") {
        if bytes
            .get(open..)
            .is_some_and(|rest| rest.starts_with(b"<!--"))
        {
            let close = find_bytes(bytes, open + 4, b"-->").ok_or("a comment is not closed")?;
            pos = close + 3;
            continue;
        }
        if !bytes.get(open + 1).is_some_and(u8::is_ascii_alphabetic) {
            pos = open + 1;
            continue;
        }
        let (mut element, after) = read_start_tag(html, open + 1)?;
        pos = after;
        if element.name == "script" || element.name == "style" {
            let (body, after_close) = read_raw_text(html, pos, &element.name)?;
            element.body = Some(body.to_owned());
            pos = after_close;
        }
        elements.push(element);
    }
    Ok(elements)
}

/// The start tag whose name begins at `start`, and the index just past its `>`.
fn read_start_tag(html: &str, start: usize) -> Result<(Element, usize)> {
    let bytes = html.as_bytes();
    let text = |from: usize, to: usize| html.get(from..to).unwrap_or_default();
    let byte = |at: usize| bytes.get(at).copied();
    let mut pos = start;
    while byte(pos).is_some_and(|b| !is_space(b) && b != b'/' && b != b'>') {
        pos += 1;
    }
    let name = text(start, pos).to_ascii_lowercase();
    let unclosed = || format!("a <{name}> tag is not closed");
    let mut attributes = Vec::new();
    loop {
        while byte(pos).is_some_and(|b| is_space(b) || b == b'/') {
            pos += 1;
        }
        match byte(pos) {
            None => return Err(unclosed()),
            Some(b'>') => break,
            Some(_) => {}
        }
        let key_start = pos;
        while byte(pos).is_some_and(|b| !is_space(b) && b != b'=' && b != b'>' && b != b'/') {
            pos += 1;
        }
        let key = text(key_start, pos).to_ascii_lowercase();
        while byte(pos).is_some_and(is_space) {
            pos += 1;
        }
        let mut value = "";
        if byte(pos) == Some(b'=') {
            pos += 1;
            while byte(pos).is_some_and(is_space) {
                pos += 1;
            }
            match byte(pos) {
                Some(quote @ (b'"' | b'\'')) => {
                    let value_start = pos + 1;
                    let end = find_bytes(bytes, value_start, &[quote]).ok_or_else(unclosed)?;
                    value = text(value_start, end);
                    pos = end + 1;
                }
                _ => {
                    let value_start = pos;
                    while byte(pos).is_some_and(|b| !is_space(b) && b != b'>') {
                        pos += 1;
                    }
                    value = text(value_start, pos);
                }
            }
        }
        attributes.push((key, value.to_owned()));
    }
    Ok((
        Element {
            name,
            attributes,
            body: None,
        },
        pos + 1,
    ))
}

/// The text from `from` up to the `</name>` that ends it (matched without regard to case), and
/// the index just past that closing tag.
fn read_raw_text<'a>(html: &'a str, from: usize, name: &str) -> Result<(&'a str, usize)> {
    let bytes = html.as_bytes();
    let unclosed = || format!("a <{name}> element is not closed");
    let mut search = from;
    loop {
        let close = find_bytes(bytes, search, b"</").ok_or_else(unclosed)?;
        let name_at = close + 2;
        let matches_name = bytes
            .get(name_at..name_at + name.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name.as_bytes()));
        let ends_name = bytes
            .get(name_at + name.len())
            .is_some_and(|&b| is_space(b) || b == b'/' || b == b'>');
        if matches_name && ends_name {
            let end = find_bytes(bytes, name_at, b">").ok_or_else(unclosed)?;
            return Ok((html.get(from..close).unwrap_or_default(), end + 1));
        }
        search = close + 2;
    }
}

/// The hashes a Content-Security-Policy needs for the inline scripts and styles of a page.
#[derive(Debug, Default, PartialEq, Eq)]
struct InlineHashes {
    scripts: Vec<String>,
    styles: Vec<String>,
}

/// `body` as a browser holds it once parsed: CRLF and a lone CR are read as LF. Nothing else
/// changes; leading and trailing newlines stay.
fn normalise_newlines(body: &str) -> String {
    body.replace("\r\n", "\n").replace('\r', "\n")
}

/// `sha256-<base64>` of `body` as a browser hashes it for a Content-Security-Policy: the text
/// between the tags, with newlines normalised as in [`normalise_newlines`].
fn csp_hash(body: &str) -> String {
    let digest = Sha256::digest(normalise_newlines(body).as_bytes());
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// The hash of every inline script and style of `html`, in page order. A script with a `src` is
/// not inline and is skipped; one with both a `src` and text is refused, as is a tag left open.
/// Elements with nothing in them are skipped.
fn inline_hashes(html: &str) -> Result<InlineHashes> {
    let mut hashes = InlineHashes::default();
    for element in scan(html)? {
        let Some(body) = element.body.as_deref() else {
            continue;
        };
        let blank = body.bytes().all(is_space);
        if element.name == "script" {
            if element.attribute("src").is_some() {
                if !blank {
                    return Err("a <script> has both a src and a body".to_owned());
                }
                continue;
            }
            if !body.is_empty() {
                hashes.scripts.push(csp_hash(body));
            }
        } else if !body.is_empty() {
            hashes.styles.push(csp_hash(body));
        }
    }
    Ok(hashes)
}

/// The `src` and `href` attributes of `html` that start with `/`, which would point outside the
/// folder the editor is served from, each written as `<tag attribute="value">`.
fn root_absolute_links(html: &str) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for element in scan(html)? {
        for (key, value) in &element.attributes {
            if (key == "src" || key == "href") && value.trim_start().starts_with('/') {
                found.push(format!("<{} {key}=\"{value}\">", element.name));
            }
        }
    }
    Ok(found)
}

// ---------------------------------------------------------------------------------------------
// The manifest

/// What goes into `editor.json`; the server's `Manifest` is what reads it.
#[derive(Debug, PartialEq, Eq)]
struct ManifestData {
    id: String,
    version: String,
    fork_repository: String,
    fork_commit: String,
    upstream_repository: String,
    upstream_commit: String,
    built: String,
    script_hashes: Vec<String>,
    style_hashes: Vec<String>,
}

/// The text of `editor.json`: indented, with a line break at the end.
fn manifest_json(data: &ManifestData) -> Result<String> {
    let value = serde_json::json!({
        "id": data.id,
        "version": data.version,
        "fork": { "repository": data.fork_repository, "commit": data.fork_commit },
        "upstream": { "repository": data.upstream_repository, "commit": data.upstream_commit },
        "built": data.built,
        "script_hashes": data.script_hashes,
        "style_hashes": data.style_hashes,
    });
    let mut text =
        serde_json::to_string_pretty(&value).map_err(|e| format!("cannot write manifest: {e}"))?;
    text.push('\n');
    Ok(text)
}

/// `version` of an app crate's `Cargo.toml`, or of the workspace's `[workspace.package]` when the
/// app says `version.workspace = true`.
fn package_version(app_manifest: &str, workspace_manifest: Option<&str>) -> Result<String> {
    let app: toml::Table = app_manifest
        .parse()
        .map_err(|e| format!("the app's Cargo.toml is not valid TOML: {e}"))?;
    let version = app
        .get("package")
        .and_then(|package| package.get("version"))
        .ok_or("the app's Cargo.toml has no package version")?;
    if let Some(text) = version.as_str() {
        return Ok(text.to_owned());
    }
    if version.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
        return Err("the app's package version is neither a string nor workspace = true".into());
    }
    let workspace: toml::Table = workspace_manifest
        .ok_or("the app's version is inherited but the checkout has no root Cargo.toml")?
        .parse()
        .map_err(|e| format!("the root Cargo.toml is not valid TOML: {e}"))?;
    workspace
        .get("workspace")
        .and_then(|table| table.get("package"))
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "the root Cargo.toml has no [workspace.package] version".to_owned())
}

/// `YYYY-MM-DD`, in UTC, of a time given as seconds since 1970-01-01.
fn date_from_unix(seconds: u64) -> String {
    // Days since 1970-01-01 to a calendar date (Howard Hinnant's `civil_from_days`).
    let days = i64::try_from(seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    date_from_unix(seconds)
}

/// What is wrong with an installed editor's manifest, given its table in `editors.toml` and the
/// text of its installed `index.html`. Empty if it matches.
fn manifest_problems(
    spec: &EditorSpec,
    manifest: &serde_json::Value,
    index_html: &str,
) -> Vec<String> {
    let text = |path: &[&str]| {
        path.iter()
            .try_fold(manifest, |value, key| value.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let list = |key: &str| -> Vec<String> {
        manifest
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut problems = Vec::new();
    if text(&["id"]) != spec.id {
        problems.push(format!(
            "its manifest names {:?}, not this editor",
            text(&["id"])
        ));
    }
    let installed = text(&["fork", "commit"]);
    if spec.commit.is_empty() {
        problems.push(format!(
            "{EDITORS_TOML} has no commit for it: record the commit it was built from ({installed})"
        ));
    } else if installed != spec.commit {
        problems.push(format!(
            "it was built from {installed} but {EDITORS_TOML} records {}",
            spec.commit
        ));
    }
    if text(&["upstream", "commit"]) != spec.base {
        problems.push(format!(
            "its upstream commit is {} but {EDITORS_TOML} has base {}",
            text(&["upstream", "commit"]),
            spec.base
        ));
    }
    match inline_hashes(index_html) {
        Err(e) => problems.push(format!("its index.html cannot be read: {e}")),
        Ok(hashes) => {
            if hashes.scripts != list("script_hashes") || hashes.styles != list("style_hashes") {
                problems.push(
                    "the inline script or style hashes in its manifest do not match its index.html"
                        .to_owned(),
                );
            }
        }
    }
    problems
}

// ---------------------------------------------------------------------------------------------
// list and verify

fn editors_dir(root: &Path) -> PathBuf {
    root.join("editors")
}

/// The first twelve characters of a commit id.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The installed manifest of `id` as JSON, or `None` if it is not installed.
fn installed_manifest(root: &Path, id: &str) -> Result<Option<serde_json::Value>> {
    let path = editors_dir(root).join(id).join(MANIFEST);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn list(root: &Path) -> Result<()> {
    for spec in read_editors(root)? {
        let recorded = if spec.commit.is_empty() {
            "not recorded".to_owned()
        } else {
            short(&spec.commit).to_owned()
        };
        let checkout = root.join(&spec.checkout);
        let state = if !checkout.is_dir() {
            "missing".to_owned()
        } else {
            match (
                git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
                git(&checkout, &["rev-parse", "HEAD"]),
                git(
                    &checkout,
                    &["status", "--porcelain", "--untracked-files=no"],
                ),
            ) {
                (Ok(branch), Ok(head), Ok(status)) => format!(
                    "{branch} at {}, {}",
                    short(&head),
                    if status.is_empty() {
                        "clean"
                    } else {
                        "uncommitted changes"
                    }
                ),
                _ => "not a Git checkout".to_owned(),
            }
        };
        let installed = match installed_manifest(root, &spec.id) {
            Ok(Some(manifest)) => {
                let field = |path: &[&str]| {
                    path.iter()
                        .try_fold(&manifest, |value, key| value.get(key))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?")
                        .to_owned()
                };
                format!(
                    "version {} from {}, built {}",
                    field(&["version"]),
                    short(&field(&["fork", "commit"])),
                    field(&["built"])
                )
            }
            Ok(None) => "not installed".to_owned(),
            Err(e) => e,
        };
        println!("{}", spec.id);
        println!("  commit recorded: {recorded}");
        println!("  checkout {}: {state}", spec.checkout);
        println!("  installed: {installed}");
    }
    Ok(())
}

/// Checks every installed editor against `editors.toml`. Editors that are not installed are not
/// checked, so a machine with no `./editors` folder passes.
pub fn verify_installed(root: &Path) -> Result<()> {
    let mut problems = Vec::new();
    let mut checked = 0;
    for spec in read_editors(root)? {
        let Some(manifest) = installed_manifest(root, &spec.id)? else {
            continue;
        };
        checked += 1;
        let index = editors_dir(root).join(&spec.id).join("index.html");
        let html = std::fs::read_to_string(&index)
            .map_err(|e| format!("cannot read {}: {e}", index.display()))?;
        for problem in manifest_problems(&spec, &manifest, &html) {
            problems.push(format!("{}: {problem}", spec.id));
        }
    }
    if problems.is_empty() {
        println!("xtask: {checked} installed editor(s) match {EDITORS_TOML}");
        Ok(())
    } else {
        Err(format!(
            "installed editors do not match:\n  {}",
            problems.join("\n  ")
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// build

/// Runs `git -C dir args…` and returns what it printed, without the trailing line break.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// A folder that is removed again when this goes out of scope.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A detached `git worktree` of a checkout, removed again when this goes out of scope, whether
/// the build worked or not.
struct Worktree {
    checkout: PathBuf,
    path: PathBuf,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = git(
            &self.checkout,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        );
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = git(&self.checkout, &["worktree", "prune"]);
    }
}

/// The first folder above `path` (or `path` itself) with a Cargo configuration in it.
fn cargo_config_above(path: &Path, cargo_home: Option<&Path>) -> Option<PathBuf> {
    path.ancestors().find_map(|dir| {
        let cargo_dir = dir.join(".cargo");
        // Cargo reads its own home for every build, so that one is not a surprise.
        if cargo_home == Some(cargo_dir.as_path()) {
            return None;
        }
        ["config.toml", "config"]
            .iter()
            .map(|name| cargo_dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Cargo's home folder: `CARGO_HOME`, else `.cargo` in the user's home folder.
fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(home));
    }
    let user = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(user).join(".cargo"))
}

/// Whether `text` could be a commit id: 7 to 40 hexadecimal characters. Anything else, such as a
/// word that starts with `-`, is not given to `git`.
fn is_commit_id(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where Cargo's output goes for the builds of editor `id`: one folder per editor, so that
/// rebuilding is incremental, under `SHARECRAFT_EDITOR_TARGET_DIR` or the system temporary folder.
fn editor_target_dir(id: &str) -> PathBuf {
    std::env::var_os("SHARECRAFT_EDITOR_TARGET_DIR")
        .map_or_else(
            || std::env::temp_dir().join("sharecraft-editor-target"),
            PathBuf::from,
        )
        .join(id)
}

fn build(root: &Path, id: &str) -> Result<()> {
    let spec = read_editors(root)?
        .into_iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| format!("{id} is not an editor in {EDITORS_TOML}"))?;

    // Refuse before touching anything.
    if !is_commit_id(&spec.base) {
        return Err(format!(
            "{EDITORS_TOML}: the base of {id} is not a commit id (7 to 40 hexadecimal characters)"
        ));
    }
    let checkout = root.join(&spec.checkout);
    if !checkout.is_dir() {
        return Err(format!(
            "the checkout {} is missing; clone {} there",
            checkout.display(),
            spec.fork
        ));
    }
    let branch = git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch != spec.branch {
        return Err(format!(
            "{} is on branch {branch}, not {}",
            checkout.display(),
            spec.branch
        ));
    }
    if !git(
        &checkout,
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        return Err(format!(
            "{} has uncommitted changes; commit them first, since the build is made from the \
             committed head",
            checkout.display()
        ));
    }
    let head = git(&checkout, &["rev-parse", "HEAD"])?;
    let is_ancestor = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["merge-base", "--is-ancestor", &spec.base, &head])
        .status()
        .map_err(|e| format!("cannot run git: {e}"))?;
    match is_ancestor.code() {
        Some(0) => {}
        Some(1) => {
            return Err(format!(
                "the base commit {} is not an ancestor of {head}; the branch was not cut from it",
                spec.base
            ));
        }
        _ => {
            return Err(format!(
                "cannot tell whether {} is an ancestor of {head}; is it fetched?",
                spec.base
            ));
        }
    }
    if !Command::new("trunk")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        return Err(format!(
            "trunk is not installed; install it for your user with:\n  {TRUNK_INSTALL}"
        ));
    }

    // Build from a clean worktree of the committed head, never in the checkout itself.
    let temp = std::env::temp_dir();
    let tag = format!("{id}-{}", short(&head));
    let worktree_path = temp.join(format!("sharecraft-editor-build-{tag}"));
    if let Some(config) = cargo_config_above(&worktree_path, cargo_home().as_deref()) {
        return Err(format!(
            "{} exists, and Cargo would apply it to the build; the build would not be the \
             fork's own. Set TMP and TEMP to a folder outside it and run again.",
            config.display()
        ));
    }
    // A killed build can leave a registration whose folder is gone.
    let _ = git(&checkout, &["worktree", "prune"]);
    if worktree_path.exists() {
        // Left behind by a build that was killed.
        let _ = git(
            &checkout,
            &[
                "worktree",
                "remove",
                "--force",
                &worktree_path.to_string_lossy(),
            ],
        );
        std::fs::remove_dir_all(&worktree_path)
            .map_err(|e| format!("cannot clear {}: {e}", worktree_path.display()))?;
        let _ = git(&checkout, &["worktree", "prune"]);
    }
    git(
        &checkout,
        &[
            "worktree",
            "add",
            "--detach",
            &worktree_path.to_string_lossy(),
            &head,
        ],
    )?;
    let worktree = Worktree {
        checkout: checkout.clone(),
        path: worktree_path,
    };

    let dist_path = temp.join(format!("sharecraft-editor-dist-{tag}"));
    let _ = std::fs::remove_dir_all(&dist_path);
    let dist = Scratch(dist_path);

    let target = editor_target_dir(id);
    println!(
        "xtask: building {id} with trunk; a build takes tens of minutes (Cargo output in {})",
        target.display()
    );
    run(Command::new("trunk")
        .current_dir(worktree.path.join(&spec.app))
        .env("CARGO_TARGET_DIR", &target)
        .args(["build", "--release", "--public-url", "./", "--dist"])
        .arg(&dist.0))?;

    let manifest = finish_build(&spec, &head, &worktree.path, &dist.0)?;
    install(root, id, &dist.0)?;
    println!(
        "xtask: installed {id} {} from {} into {}",
        manifest.version,
        short(&head),
        editors_dir(root).join(id).display()
    );
    if spec.commit != head {
        println!(
            "xtask: {EDITORS_TOML} records {:?} for {id}, not this commit",
            spec.commit
        );
    }
    Ok(())
}

/// Checks the finished build in `dist`, then adds `editor.json` and the fork's licence files.
fn finish_build(
    spec: &EditorSpec,
    head: &str,
    worktree: &Path,
    dist: &Path,
) -> Result<ManifestData> {
    let index = dist.join("index.html");
    let html = std::fs::read_to_string(&index)
        .map_err(|e| format!("the build has no readable index.html: {e}"))?;
    let links = root_absolute_links(&html)?;
    if !links.is_empty() {
        return Err(format!(
            "the build's index.html points outside its own folder:\n  {}",
            links.join("\n  ")
        ));
    }
    let hashes = inline_hashes(&html)?;

    let read = |path: PathBuf| std::fs::read_to_string(path).ok();
    let app_manifest = read(worktree.join(&spec.app).join("Cargo.toml"))
        .ok_or_else(|| format!("cannot read {}/Cargo.toml", spec.app))?;
    let version = package_version(&app_manifest, read(worktree.join("Cargo.toml")).as_deref())?;

    let data = ManifestData {
        id: spec.id.clone(),
        version,
        fork_repository: spec.fork.clone(),
        fork_commit: head.to_owned(),
        upstream_repository: spec.upstream.clone(),
        upstream_commit: spec.base.clone(),
        built: today(),
        script_hashes: hashes.scripts,
        style_hashes: hashes.styles,
    };
    let json = manifest_json(&data)?;
    std::fs::write(dist.join(MANIFEST), json)
        .map_err(|e| format!("cannot write {MANIFEST}: {e}"))?;

    for name in collect_licences(worktree, dist)? {
        println!("xtask: the build has its own licenses/{name}, which differs; kept it");
    }
    Ok(data)
}

/// The licence and attribution files among `names`, found by [`LICENCE_PREFIXES`].
fn licence_files(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            LICENCE_PREFIXES
                .iter()
                .any(|prefix| lower.starts_with(prefix))
        })
        .cloned()
        .collect()
}

/// Copies the licence and attribution files in the root of `worktree` into `<dist>/licenses/`,
/// and returns the names of those the build already had, with other content, which are kept.
/// Fails unless a LICENSE (or LICENCE) file and a NOTICE file were found.
fn collect_licences(worktree: &Path, dist: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(worktree)
        .map_err(|e| format!("cannot list {}: {e}", worktree.display()))?
    {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", worktree.display()))?;
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    let found = licence_files(&names);
    let has = |prefixes: &[&str]| {
        found.iter().any(|name| {
            let lower = name.to_ascii_lowercase();
            prefixes.iter().any(|prefix| lower.starts_with(prefix))
        })
    };
    if !has(&["license", "licence"]) {
        return Err("the fork has no LICENSE file in its root; the build needs one".to_owned());
    }
    if !has(&["notice"]) {
        return Err("the fork has no NOTICE file in its root; the build needs one".to_owned());
    }
    let licences = dist.join("licenses");
    std::fs::create_dir_all(&licences)
        .map_err(|e| format!("cannot create {}: {e}", licences.display()))?;
    let mut kept = Vec::new();
    for name in found {
        let from = worktree.join(&name);
        let to = licences.join(&name);
        if to.exists() {
            if std::fs::read(&from).ok() != std::fs::read(&to).ok() {
                kept.push(name);
            }
            continue;
        }
        std::fs::copy(&from, &to).map_err(|e| format!("cannot copy {name}: {e}"))?;
    }
    Ok(kept)
}

/// Replaces `./editors/<id>` with the folder `built`: the new editor is written beside the old
/// one first, so a failure leaves the old one in place.
fn install(root: &Path, id: &str, built: &Path) -> Result<()> {
    let editors = editors_dir(root);
    std::fs::create_dir_all(&editors)
        .map_err(|e| format!("cannot create {}: {e}", editors.display()))?;
    let target = editors.join(id);
    let new = editors.join(format!("{id}.new"));
    let old = editors.join(format!("{id}.old"));
    for leftover in [&new, &old] {
        if leftover.exists() {
            std::fs::remove_dir_all(leftover)
                .map_err(|e| format!("cannot clear {}: {e}", leftover.display()))?;
        }
    }
    if let Err(e) = copy_dir(built, &new) {
        let _ = std::fs::remove_dir_all(&new);
        return Err(e);
    }
    let had_old = target.exists();
    if had_old && let Err(e) = std::fs::rename(&target, &old) {
        let _ = std::fs::remove_dir_all(&new);
        return Err(format!("cannot move {} aside: {e}", target.display()));
    }
    if let Err(e) = std::fs::rename(&new, &target) {
        if had_old {
            let _ = std::fs::rename(&old, &target);
        }
        let _ = std::fs::remove_dir_all(&new);
        return Err(format!("cannot install {}: {e}", target.display()));
    }
    if had_old && let Err(e) = std::fs::remove_dir_all(&old) {
        eprintln!(
            "xtask: warning: installed, but cannot delete the old build {}: {e}",
            old.display()
        );
    }
    Ok(())
}

/// Copies the folder `from`, with everything in it, to `to`.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let failed =
        |e: std::io::Error| format!("cannot copy {} to {}: {e}", from.display(), to.display());
    std::fs::create_dir_all(to).map_err(failed)?;
    for entry in std::fs::read_dir(from).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let destination = to.join(entry.file_name());
        if entry.file_type().map_err(failed)?.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), &destination).map_err(failed)?;
        }
    }
    Ok(())
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

    const MODULE_SCRIPT: &str = "\nimport init from './app.js';\ninit();\n";
    const NON_ASCII_SCRIPT: &str = "caf\u{e9} \u{2603} \u{65e5}\u{672c}\u{8a9e}";

    fn page() -> String {
        [
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<link rel=\"stylesheet\" href=\"./app.css\">",
            "<style>body{margin:0}</style>",
            "<!-- <script>not a script</script> -->",
            "<script type=\"module\">",
            "import init from './app.js';",
            "init();",
            "</script>",
            "<script>caf\u{e9} \u{2603} \u{65e5}\u{672c}\u{8a9e}</SCRIPT >",
            "<script src=\"./wasm-loader.js\"></script>",
            "</head><body><a href=\"./about.html\">x</a></body></html>",
        ]
        .join("\n")
    }

    #[test]
    fn inline_hashes_match_known_values() {
        // Hashes computed with Python's hashlib over the UTF-8 bytes, not with this code.
        let hashes = inline_hashes(&page()).unwrap();
        assert_eq!(
            hashes.scripts,
            [
                // Leading and trailing newlines count.
                "sha256-O5JTT//82Kx49BrvVhdpg+8xboXFwBQ1Tq7v8lYWyX4=",
                "sha256-bQWdfyoQMpGkYHLX3mTFe1nPKANGrsMxb1dQazFSozQ=",
            ]
        );
        assert_eq!(
            hashes.styles,
            ["sha256-IAdwN3biDCQ3brrgp1m8kBEsPQYyqfREKOEfeoREopc="]
        );
        assert_eq!(
            csp_hash(MODULE_SCRIPT),
            "sha256-O5JTT//82Kx49BrvVhdpg+8xboXFwBQ1Tq7v8lYWyX4="
        );
        assert_eq!(
            csp_hash(NON_ASCII_SCRIPT),
            "sha256-bQWdfyoQMpGkYHLX3mTFe1nPKANGrsMxb1dQazFSozQ="
        );
    }

    #[test]
    fn line_breaks_and_case_are_hashed_as_they_are() {
        let hashes =
            inline_hashes("<SCRIPT>UPPER();</Script><style>a{color:red}\n</style>").unwrap();
        assert_eq!(
            hashes.scripts,
            ["sha256-KVhMItq4XmdqnlOv+L7aZLM91kSiOfjzEqayk4mgEh0="]
        );
        assert_eq!(
            hashes.styles,
            ["sha256-8e/a53E8pD0reBM7qquiWvKITpZgFpBiFcZDadvq2JA="]
        );
        // A browser turns CRLF and a lone CR into LF before the text exists; the newlines at the
        // start and end are kept. Hash of "\nx\n" from Python's hashlib.
        let lf = "sha256-0j+bQHb1Xx5lDFk/gPRR8glNxZzORwwGPrIq6mPkyzk=";
        for page in [
            "<script>\nx\n</script>",
            "<script>\r\nx\r\n</script>",
            "<script>\rx\r</script>",
        ] {
            assert_eq!(inline_hashes(page).unwrap().scripts, [lf], "{page:?}");
        }
    }

    #[test]
    fn external_scripts_are_not_hashed() {
        let hashes =
            inline_hashes("<script src=\"./a.js\"></script><script src=./b.js> </script>").unwrap();
        assert_eq!(hashes, InlineHashes::default());
        let both = inline_hashes("<script src=\"./a.js\">init();</script>");
        assert!(both.unwrap_err().contains("both a src and a body"));
    }

    #[test]
    fn unclosed_tags_are_refused() {
        for bad in [
            "<script>init();",
            "<style>a{}",
            "<script",
            "<script src=\"x",
            "<!-- never closed",
            "<script>x</script",
        ] {
            assert!(inline_hashes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn root_absolute_links_are_refused() {
        let html = "<link href=\"/app.css\"><script src='/app.js'></script>\
                    <img src=\"//cdn.example/x.png\"><a href=\"./ok.html\">\
                    <script>var s = '<a href=\"/in-script\">';</script>";
        assert_eq!(
            root_absolute_links(html).unwrap(),
            [
                "<link href=\"/app.css\">",
                "<script src=\"/app.js\">",
                "<img src=\"//cdn.example/x.png\">",
            ]
        );
        assert!(root_absolute_links(&page()).unwrap().is_empty());
    }

    const EDITORS: &str = r#"
[gridcraft]
upstream = "https://github.com/storytold/gridcraft"
base     = "e9addf52fe68847e6c1310fcfa5d42b7aa8b9650"
fork     = "https://github.com/ThineCraft/GridCraft"
branch   = "sharecraft"
commit   = ""
app      = "apps/gridcraft-web"
checkout = "../GridCraft"
"#;

    #[test]
    fn editors_toml_parses() {
        let editors = parse_editors(EDITORS).unwrap();
        assert_eq!(
            editors,
            [EditorSpec {
                id: "gridcraft".to_owned(),
                upstream: "https://github.com/storytold/gridcraft".to_owned(),
                base: "e9addf52fe68847e6c1310fcfa5d42b7aa8b9650".to_owned(),
                fork: "https://github.com/ThineCraft/GridCraft".to_owned(),
                branch: "sharecraft".to_owned(),
                commit: String::new(),
                app: "apps/gridcraft-web".to_owned(),
                checkout: "../GridCraft".to_owned(),
            }]
        );
        let missing = parse_editors("[a]\nupstream = \"x\"\n");
        assert!(missing.unwrap_err().contains("[a] needs a string `base`"));
        assert!(parse_editors("not toml [").is_err());
    }

    #[test]
    fn the_real_editors_toml_lists_the_four_editors() {
        let text = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(EDITORS_TOML),
        )
        .unwrap();
        let editors = parse_editors(&text).unwrap();
        let ids: Vec<&str> = editors.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["gridcraft", "pdfcraft", "solidcraft", "wordcraft"]);
        assert!(editors.iter().all(|e| e.branch == "sharecraft"));
    }

    fn fixed_manifest() -> ManifestData {
        ManifestData {
            id: "gridcraft".to_owned(),
            version: "0.3.1".to_owned(),
            fork_repository: "https://github.com/ThineCraft/GridCraft".to_owned(),
            fork_commit: "1111111111111111111111111111111111111111".to_owned(),
            upstream_repository: "https://github.com/storytold/gridcraft".to_owned(),
            upstream_commit: "e9addf52fe68847e6c1310fcfa5d42b7aa8b9650".to_owned(),
            built: "2026-11-10".to_owned(),
            script_hashes: vec!["sha256-O5JTT//82Kx49BrvVhdpg+8xboXFwBQ1Tq7v8lYWyX4=".to_owned()],
            style_hashes: vec!["sha256-IAdwN3biDCQ3brrgp1m8kBEsPQYyqfREKOEfeoREopc=".to_owned()],
        }
    }

    /// The server reads this same file in its own tests (`editors.rs`), so what is written here
    /// is accepted there.
    #[test]
    fn manifest_round_trips_through_the_server_type() {
        let fixture = include_str!("../tests/fixtures/editor.json");
        assert_eq!(
            manifest_json(&fixed_manifest()).unwrap(),
            fixture.replace("\r\n", "\n")
        );
    }

    #[test]
    fn versions_come_from_the_app_or_the_workspace() {
        assert_eq!(
            package_version("[package]\nname = \"a\"\nversion = \"1.2.3\"\n", None).unwrap(),
            "1.2.3"
        );
        let inherited = "[package]\nname = \"a\"\nversion.workspace = true\n";
        let workspace = "[workspace.package]\nversion = \"4.5.6\"\n";
        assert_eq!(
            package_version(inherited, Some(workspace)).unwrap(),
            "4.5.6"
        );
        assert!(package_version(inherited, None).is_err());
        assert!(package_version(inherited, Some("[workspace]\n")).is_err());
        assert!(package_version("[package]\nname = \"a\"\n", None).is_err());
    }

    #[test]
    fn dates_are_calendar_dates() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(951_782_400), "2000-02-29");
        assert_eq!(date_from_unix(1_794_268_800), "2026-11-10");
    }

    #[test]
    fn an_installed_manifest_is_checked_against_editors_toml() {
        let mut spec = parse_editors(EDITORS).unwrap().remove(0);
        let json = serde_json::from_str(&manifest_json(&fixed_manifest()).unwrap()).unwrap();
        let matching = "<style>body{margin:0}</style>\n<script>\nimport init from './app.js';\ninit();\n</script>";
        let other = "<style>body{margin:0}</style><script>x</script>";
        // Nothing recorded yet.
        let problems = manifest_problems(&spec, &json, matching);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("record the commit"));
        // Recorded, and the same.
        spec.commit = "1111111111111111111111111111111111111111".to_owned();
        assert!(manifest_problems(&spec, &json, matching).is_empty());
        // A different commit, and a page that no longer matches the hashes.
        spec.commit = "2222222222222222222222222222222222222222".to_owned();
        let problems = manifest_problems(&spec, &json, other);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_cargo_config_above_the_build_folder_is_found() {
        let dir = scratch("cargo-config");
        let inner = dir.join("a").join("b");
        std::fs::create_dir_all(&inner).unwrap();
        let ours = |found: Option<PathBuf>| found.filter(|p| p.starts_with(&dir));
        assert_eq!(ours(cargo_config_above(&inner, None)), None);
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join(".cargo").join("config.toml"), "").unwrap();
        assert_eq!(
            cargo_config_above(&inner, None),
            Some(dir.join(".cargo").join("config.toml"))
        );
        // The user's own cargo home is read by every build anyway, so it is not a reason.
        assert_eq!(
            ours(cargo_config_above(&inner, Some(&dir.join(".cargo")))),
            None
        );
        // Another folder's .cargo still counts.
        std::fs::create_dir_all(dir.join("a").join(".cargo")).unwrap();
        std::fs::write(dir.join("a").join(".cargo").join("config"), "").unwrap();
        assert_eq!(
            cargo_config_above(&inner, Some(&dir.join(".cargo"))),
            Some(dir.join("a").join(".cargo").join("config"))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_base_that_could_be_an_option_is_refused() {
        assert!(is_commit_id("e9addf52fe68847e6c1310fcfa5d42b7aa8b9650"));
        assert!(is_commit_id("e9addf5"));
        for bad in ["", "e9addf", "--all", "-h", "e9addf5 --x", "zzzzzzzz"] {
            assert!(!is_commit_id(bad), "{bad}");
        }
    }

    fn write_files(dir: &Path, names: &[&str]) {
        for name in names {
            std::fs::write(dir.join(name), name).unwrap();
        }
    }

    #[test]
    fn licence_files_are_found_by_prefix() {
        let names: Vec<String> = [
            "LICENSE-MIT",
            "license.txt",
            "Licence",
            "NOTICE.md",
            "notice",
            "ATTRIBUTION.toml",
            "COPYING",
            "LICENSES.toml",
            "THIRD-PARTY-LICENSES.txt",
            "README.md",
            "Cargo.toml",
            "unlicensed.txt",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            licence_files(&names),
            [
                "LICENSE-MIT",
                "license.txt",
                "Licence",
                "NOTICE.md",
                "notice",
                "ATTRIBUTION.toml",
                "COPYING",
                "LICENSES.toml",
                "THIRD-PARTY-LICENSES.txt"
            ]
        );
    }

    #[test]
    fn licences_are_copied_and_a_missing_one_fails_the_build() {
        let tree = scratch("licences-tree");
        let dist = scratch("licences-dist");
        write_files(
            &tree,
            &["LICENSE-MIT", "NOTICE", "ATTRIBUTION.md", "Cargo.toml"],
        );
        std::fs::create_dir(tree.join("LICENSES")).unwrap();
        // The build already has a different NOTICE: it is kept.
        std::fs::create_dir_all(dist.join("licenses")).unwrap();
        std::fs::write(dist.join("licenses").join("NOTICE"), "from the build").unwrap();
        let kept = collect_licences(&tree, &dist).unwrap();
        assert_eq!(kept, ["NOTICE"]);
        let copied = |name: &str| std::fs::read_to_string(dist.join("licenses").join(name));
        assert_eq!(copied("LICENSE-MIT").unwrap(), "LICENSE-MIT");
        assert_eq!(copied("ATTRIBUTION.md").unwrap(), "ATTRIBUTION.md");
        assert_eq!(copied("NOTICE").unwrap(), "from the build");
        assert!(!dist.join("licenses").join("LICENSES").exists());
        assert!(!dist.join("licenses").join("Cargo.toml").exists());

        std::fs::remove_file(tree.join("NOTICE")).unwrap();
        let error = collect_licences(&tree, &dist).unwrap_err();
        assert!(error.contains("NOTICE"), "{error}");
        std::fs::remove_file(tree.join("LICENSE-MIT")).unwrap();
        std::fs::write(tree.join("NOTICE"), "n").unwrap();
        let error = collect_licences(&tree, &dist).unwrap_err();
        assert!(error.contains("LICENSE"), "{error}");
        std::fs::remove_dir_all(&tree).unwrap();
        std::fs::remove_dir_all(&dist).unwrap();
    }

    #[test]
    fn install_replaces_the_old_build_and_clears_leftovers() {
        let root = scratch("install-root");
        let built = scratch("install-built");
        std::fs::write(built.join("index.html"), "new").unwrap();
        let editors = root.join("editors");
        std::fs::create_dir_all(editors.join("gridcraft")).unwrap();
        std::fs::write(editors.join("gridcraft").join("index.html"), "old").unwrap();
        std::fs::write(editors.join("gridcraft").join("only-old.txt"), "old").unwrap();
        // Left by an earlier run that failed.
        std::fs::create_dir_all(editors.join("gridcraft.new")).unwrap();
        std::fs::write(editors.join("gridcraft.new").join("stale.txt"), "stale").unwrap();

        install(&root, "gridcraft", &built).unwrap();
        let read = |name: &str| std::fs::read_to_string(editors.join("gridcraft").join(name));
        assert_eq!(read("index.html").unwrap(), "new");
        assert!(read("only-old.txt").is_err());
        assert!(read("stale.txt").is_err());
        assert!(!editors.join("gridcraft.new").exists());
        assert!(!editors.join("gridcraft.old").exists());

        // And a first install, with nothing to replace.
        install(&root, "wordcraft", &built).unwrap();
        assert!(editors.join("wordcraft").join("index.html").is_file());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&built).unwrap();
    }
}
