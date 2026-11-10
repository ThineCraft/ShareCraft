//! Installed editors: the built web apps of the file editors (spreadsheet, word processor, PDF,
//! CAD), served from the server's own origin so that their browser bridge works.
//!
//! Each editor lives in `<editors_dir>/<id>/`, with a manifest `editor.json` beside its files.
//! The manifest is read once, when the server starts.

use std::collections::BTreeMap;
use std::path::Path;

use axum::Json;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use records::Editor;
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::{Signed, same_origin};

/// The name of an editor's manifest, in the editor's folder.
const MANIFEST: &str = "editor.json";

/// A Git repository and the commit a build was made from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Where the source is.
    pub repository: String,
    /// The commit that was built.
    pub commit: String,
}

/// What `cargo xtask editors build` wrote about an installed editor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The editor's id: the folder name, and an [`Editor`] id other than `text`.
    pub id: String,
    /// The editor's own version.
    pub version: String,
    /// The fork the editor was built from.
    pub fork: Source,
    /// The upstream project the fork follows.
    pub upstream: Source,
    /// The date of the build, `YYYY-MM-DD`.
    pub built: String,
    /// `sha256-…` of each inline script in `index.html`.
    pub script_hashes: Vec<String>,
    /// `sha256-…` of each inline style in `index.html`.
    pub style_hashes: Vec<String>,
}

impl Manifest {
    /// The Content-Security-Policy of this editor's files: its own files, WebAssembly, and the
    /// inline scripts and styles its manifest names by hash.
    fn policy(&self) -> String {
        let quoted = |hashes: &[String]| {
            hashes
                .iter()
                .map(|hash| format!(" '{hash}'"))
                .collect::<String>()
        };
        format!(
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{}; style-src 'self'{}; \
             img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; \
             worker-src 'self' blob:; frame-src 'self' blob:; frame-ancestors 'self'",
            quoted(&self.script_hashes),
            quoted(&self.style_hashes),
        )
    }
}

/// The editors installed when the server started.
#[derive(Debug, Default)]
pub struct Editors {
    by_id: BTreeMap<String, Manifest>,
}

impl Editors {
    /// Reads the manifests under `dir`. Never fails: a missing directory means no editors, and a
    /// folder whose manifest is missing, unreadable, invalid or names another id is skipped, with
    /// one warning line on standard error naming the folder.
    #[must_use]
    pub fn load(dir: &Path) -> Editors {
        let mut by_id = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Editors { by_id };
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().into_owned();
            match read_manifest(&entry.path(), &folder) {
                Ok(manifest) => {
                    by_id.insert(folder, manifest);
                }
                Err(reason) => {
                    eprintln!("sharecraft-server: editor folder {folder:?} skipped: {reason}");
                }
            }
        }
        Editors { by_id }
    }

    /// The installed editor with this id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Manifest> {
        self.by_id.get(id)
    }

    /// The installed editors, by id.
    #[must_use]
    pub fn list(&self) -> Vec<&Manifest> {
        self.by_id.values().collect()
    }
}

/// The manifest in `folder_path`, checked against the folder's name.
fn read_manifest(folder_path: &Path, folder: &str) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(folder_path.join(MANIFEST))
        .map_err(|e| format!("cannot read {MANIFEST}: {}", e.kind()))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("{MANIFEST} is not valid: {e}"))?;
    if manifest.id != folder {
        return Err(format!("{MANIFEST} names another editor"));
    }
    if !Editor::from_id(&manifest.id).is_some_and(Editor::is_file) {
        return Err("not the id of a file editor".to_owned());
    }
    for hash in manifest.script_hashes.iter().chain(&manifest.style_hashes) {
        if !is_hash(hash) {
            return Err(format!("{MANIFEST} has a hash that is not sha256-<base64>"));
        }
    }
    Ok(manifest)
}

/// Whether `text` is `sha256-` followed by base64 characters only.
fn is_hash(text: &str) -> bool {
    text.strip_prefix("sha256-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    })
}

/// One row of `GET /api/editors`.
#[derive(Serialize)]
pub(crate) struct EditorSummary {
    id: String,
    version: String,
    built: String,
}

/// `GET /api/editors`: the installed editors, for a signed-in caller.
pub(crate) async fn list(
    State(state): State<AppState>,
    _signed: Signed,
) -> Json<Vec<EditorSummary>> {
    Json(
        state
            .editors()
            .list()
            .into_iter()
            .map(|manifest| EditorSummary {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                built: manifest.built.clone(),
            })
            .collect(),
    )
}

/// `GET /editors/{id}/…`: the static files of an installed editor, for a signed-in caller.
pub(crate) fn router(state: AppState) -> Router<AppState> {
    let files = ServeDir::new(&state.config().editors_dir);
    Router::new()
        .nest_service("/editors", files)
        .layer(middleware::from_fn_with_state(state, guard))
}

/// Lets a request through to the editor's files only if the caller is signed in, the editor is
/// installed and the path stays inside its folder; everything else is a plain `404`. Answers
/// carry the editor's policy instead of the site's.
async fn guard(
    State(state): State<AppState>,
    signed: Result<Signed, ApiError>,
    request: Request,
    next: Next,
) -> Response {
    match signed {
        Ok(_) => {}
        Err(ApiError::Unavailable) => return ApiError::Unavailable.into_response(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }
    if !same_origin(request.headers()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some((id, rest)) = split_path(request.uri().path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(manifest) = state.editors().get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !stays_inside(rest) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(policy) = HeaderValue::from_str(&manifest.policy()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, policy);
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// The editor id and the rest of the path after it (without the slash that separates them) in
/// `/editors/<id>[/<rest>]`.
fn split_path(path: &str) -> Option<(&str, &str)> {
    let after = path.strip_prefix("/editors/")?;
    Some(after.split_once('/').unwrap_or((after, "")))
}

/// Whether `rest`, a still percent-encoded path below an editor's folder, names something inside
/// it: no segment is empty (except a last one), `.` or `..`, and none decodes to something with a
/// separator, a drive colon or a NUL, however it is encoded.
fn stays_inside(rest: &str) -> bool {
    let mut segments = rest.split('/').peekable();
    while let Some(raw) = segments.next() {
        if raw.is_empty() {
            if segments.peek().is_some() {
                return false;
            }
            continue;
        }
        let Some(segment) = percent_decode(raw) else {
            return false;
        };
        if segment == "." || segment == ".." {
            return false;
        }
        if segment.contains(['/', '\\', ':', '\0']) {
            return false;
        }
    }
    true
}

/// `text` with its `%XX` escapes decoded; `None` for a malformed escape or bytes that are not
/// UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(b) = input.next() {
        if b == b'%' {
            let high = hex_value(input.next()?)?;
            let low = hex_value(input.next()?)?;
            bytes.push(high << 4 | low);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
    fn hashes_are_sha256_and_base64() {
        assert!(is_hash("sha256-abc+/="));
        assert!(!is_hash("sha256-"));
        assert!(!is_hash("sha512-abc"));
        assert!(!is_hash("sha256-a' 'unsafe-inline"));
        assert!(!is_hash("sha256-a;b"));
    }

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        for bad in [
            "..", "../x", "a/../x", ".", "%2e%2e/x", "%2E%2e", "a%2fb", "a%5cb", "..%5cx", "a//b",
            "c:", "%00", "%zz", "%ff",
        ] {
            assert!(!stays_inside(bad), "{bad}");
        }
        for good in ["", "index.html", "a/b.wasm", "a/", "caf%C3%A9.js"] {
            assert!(stays_inside(good), "{good}");
        }
    }

    #[test]
    fn the_path_splits_into_id_and_rest() {
        assert_eq!(
            split_path("/editors/gridcraft/a/b"),
            Some(("gridcraft", "a/b"))
        );
        assert_eq!(split_path("/editors/gridcraft/"), Some(("gridcraft", "")));
        assert_eq!(split_path("/editors/gridcraft"), Some(("gridcraft", "")));
        assert_eq!(split_path("/other"), None);
    }

    /// The manifest `cargo xtask editors build` writes (a fixture shared with xtask's tests) is
    /// accepted by the server.
    #[test]
    fn the_manifest_xtask_writes_is_accepted() {
        let folder = std::env::temp_dir().join(format!("manifest-fixture-{}", std::process::id()));
        let editor = folder.join("gridcraft");
        std::fs::create_dir_all(&editor).unwrap();
        std::fs::write(
            editor.join(MANIFEST),
            include_str!("../../../xtask/tests/fixtures/editor.json"),
        )
        .unwrap();
        let manifest = read_manifest(&editor, "gridcraft");
        std::fs::remove_dir_all(&folder).unwrap();
        let manifest = manifest.unwrap();
        assert_eq!(manifest.id, "gridcraft");
        assert_eq!(manifest.script_hashes.len(), 1);
    }
}
