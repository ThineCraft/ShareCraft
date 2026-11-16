//! A fake GitHub REST API for tests: an axum app on `127.0.0.1:0` serving one repository from
//! an in-memory model, with the endpoints the GitHub client uses, ETags and `304`, tarballs
//! built with the real `tar` and `flate2`, and switches that force GitHub's failures.
//!
//! It stops accepting connections when dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::StreamExt;
use time::OffsetDateTime;

/// The author name every commit of the fake carries.
pub const AUTHOR_NAME: &str = "Octo Cat";

/// A commit the fake made: its id and its root tree's id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitId {
    pub sha: String,
    pub tree: String,
}

/// One entry of a commit's tree, as a test describes it.
#[derive(Clone, Debug)]
pub enum FakeEntry {
    /// A regular file: its path and bytes.
    File(String, Vec<u8>),
    /// A symbolic link (mode 120000): its path and target.
    Symlink(String, String),
    /// A submodule (type `commit`): its path and the commit it pins.
    Submodule(String, String),
}

/// One row of a recursive tree listing.
#[derive(Clone, Debug)]
struct TreeRow {
    path: String,
    mode: &'static str,
    kind: &'static str,
    sha: String,
    size: Option<u64>,
}

/// The repository the fake serves.
#[derive(Default)]
pub struct FakeRepo {
    /// Branch name to commit id.
    pub branches: BTreeMap<String, String>,
    /// Commit id to its tree id and entries.
    commits: HashMap<String, (String, Vec<FakeEntry>)>,
    /// Root tree id to its recursive listing.
    trees: HashMap<String, Vec<TreeRow>>,
    /// Blob id to bytes.
    pub blobs: HashMap<String, Vec<u8>>,
}

#[derive(Default)]
struct Model {
    owner: String,
    name: String,
    repo: FakeRepo,
    /// The one token that is valid; the repository is private when set.
    token: Option<String>,
    size_kb: Option<u64>,
    made: u64,
    truncate_trees: bool,
    hold_open_blobs: bool,
    /// The reset time (seconds since 1970) for the next request's primary rate-limit answer.
    rate_limit_next: Option<i64>,
    /// The message of a bare 403 the next request is answered with.
    forbid_next: Option<String>,
    /// Seconds for the next request's `429` with `retry-after`.
    retry_after_next: Option<u64>,
    /// A status the next request is answered with; its body echoes the `Authorization` header.
    fail_next: Option<u16>,
    /// Where tarball downloads are redirected; this fake itself when unset.
    redirect_tarballs_to: Option<String>,
    counts: HashMap<String, usize>,
    last_headers: HeaderMap,
}

/// A running fake GitHub. Dropping it stops it.
pub struct FakeGithub {
    pub addr: SocketAddr,
    model: Arc<Mutex<Model>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The Git object id of `kind` with `data`, lower-case hex, computed with gix directly (not
/// with the code under test).
fn object_id(kind: gix::objs::Kind, data: &[u8]) -> String {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, kind, data)
        .unwrap()
        .to_string()
}

impl FakeGithub {
    /// Starts a fake serving the empty public repository `owner/name`.
    pub async fn start(owner: &str, name: &str) -> FakeGithub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let model = Arc::new(Mutex::new(Model {
            owner: owner.to_owned(),
            name: name.to_owned(),
            ..Model::default()
        }));
        let app = Router::new().fallback(handle).with_state(Shared {
            model: model.clone(),
            addr,
        });
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeGithub { addr, model, task }
    }

    /// `http://127.0.0.1:<port>`, the API base address to give the client.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Commits `files` (path, bytes) as the whole tree of `branch`, creating the branch.
    pub fn commit(&self, branch: &str, files: &[(&str, Vec<u8>)]) -> CommitId {
        let entries = files
            .iter()
            .map(|(path, bytes)| FakeEntry::File((*path).to_owned(), bytes.clone()))
            .collect();
        self.commit_entries(branch, entries)
    }

    /// Commits `entries` as the whole tree of `branch`, creating the branch.
    pub fn commit_entries(&self, branch: &str, entries: Vec<FakeEntry>) -> CommitId {
        let mut model = self.model.lock().unwrap();
        model.made += 1;
        let mut rows = Vec::new();
        let mut dirs = BTreeSet::new();
        for entry in &entries {
            let (path, row) = match entry {
                FakeEntry::File(path, bytes) => {
                    let sha = object_id(gix::objs::Kind::Blob, bytes);
                    model.repo.blobs.insert(sha.clone(), bytes.clone());
                    let size = Some(bytes.len() as u64);
                    (path, ("100644", "blob", sha, size))
                }
                FakeEntry::Symlink(path, target) => {
                    let sha = object_id(gix::objs::Kind::Blob, target.as_bytes());
                    model
                        .repo
                        .blobs
                        .insert(sha.clone(), target.as_bytes().to_vec());
                    let size = Some(target.len() as u64);
                    (path, ("120000", "blob", sha, size))
                }
                FakeEntry::Submodule(path, sha) => (path, ("160000", "commit", sha.clone(), None)),
            };
            let mut parent = path.as_str();
            while let Some((dir, _)) = parent.rsplit_once('/') {
                dirs.insert(dir.to_owned());
                parent = dir;
            }
            let (mode, kind, sha, size) = row;
            rows.push(TreeRow {
                path: path.clone(),
                mode,
                kind,
                sha,
                size,
            });
        }
        for dir in &dirs {
            let listing: String = rows
                .iter()
                .filter(|row| row.path.starts_with(&format!("{dir}/")))
                .map(|row| format!("{} {}\n", row.path, row.sha))
                .collect();
            rows.push(TreeRow {
                path: dir.clone(),
                mode: "040000",
                kind: "tree",
                sha: object_id(gix::objs::Kind::Tree, listing.as_bytes()),
                size: None,
            });
        }
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        let listing: String = rows
            .iter()
            .map(|row| format!("{} {}\n", row.path, row.sha))
            .collect();
        let tree = object_id(gix::objs::Kind::Tree, listing.as_bytes());
        let previous = model.repo.branches.get(branch).cloned().unwrap_or_default();
        let commit_text = format!("tree {tree}\nparent {previous}\nn {}\n", model.made);
        let sha = object_id(gix::objs::Kind::Commit, commit_text.as_bytes());
        model.repo.trees.insert(tree.clone(), rows);
        model
            .repo
            .commits
            .insert(sha.clone(), (tree.clone(), entries));
        model.repo.branches.insert(branch.to_owned(), sha.clone());
        CommitId { sha, tree }
    }

    /// Points `branch` at the earlier commit `sha`, as a force-push back would.
    pub fn reset_branch(&self, branch: &str, sha: &str) {
        let mut model = self.model.lock().unwrap();
        assert!(model.repo.commits.contains_key(sha), "no such commit");
        model
            .repo
            .branches
            .insert(branch.to_owned(), sha.to_owned());
    }

    /// Deletes `branch`; its commits stay readable by id.
    pub fn delete_branch(&self, branch: &str) {
        self.model.lock().unwrap().repo.branches.remove(branch);
    }

    /// Every tree listing from now on is truncated, as GitHub's is for a very large tree.
    pub fn force_truncate_trees(&self) {
        self.model.lock().unwrap().truncate_trees = true;
    }

    /// Makes the repository private, readable only with `token`; any other token is refused
    /// with `401`, and no token gets `404`, as GitHub answers.
    pub fn require_token(&self, token: &str) {
        self.model.lock().unwrap().token = Some(token.to_owned());
    }

    /// The next request is refused by the primary rate limit, which resets at `reset`.
    pub fn rate_limit_next(&self, reset: OffsetDateTime) {
        self.model.lock().unwrap().rate_limit_next = Some(reset.unix_timestamp());
    }

    /// Like [`FakeGithub::rate_limit_next`], with the reset given as raw seconds since 1970,
    /// however far off.
    pub fn rate_limit_next_epoch(&self, reset: i64) {
        self.model.lock().unwrap().rate_limit_next = Some(reset);
    }

    /// The next request is answered with a bare 403 (no rate-limit headers) whose message is
    /// `message`.
    pub fn forbid_next(&self, message: &str) {
        self.model.lock().unwrap().forbid_next = Some(message.to_owned());
    }

    /// The next request is refused by a secondary rate limit: `429` with `retry-after`.
    pub fn retry_after_next(&self, seconds: u64) {
        self.model.lock().unwrap().retry_after_next = Some(seconds);
    }

    /// The next request is answered with `status`, its body echoing the `Authorization`
    /// header it carried.
    pub fn fail_next(&self, status: u16) {
        self.model.lock().unwrap().fail_next = Some(status);
    }

    /// The blob endpoint serves `sha` with its first byte changed from now on (same length), as
    /// a corrupted download would arrive. Tarballs are unaffected.
    pub fn corrupt_blob(&self, sha: &str) {
        let mut model = self.model.lock().unwrap();
        let bytes = model.repo.blobs.get_mut(sha).unwrap();
        bytes[0] ^= 0xff;
    }

    /// Blob bodies from now on are sent in full but never end.
    pub fn hold_open_blob_bodies(&self) {
        self.model.lock().unwrap().hold_open_blobs = true;
    }

    /// Tarball downloads are redirected to `origin` (another server) instead of this one.
    pub fn redirect_tarballs_to(&self, origin: &str) {
        self.model.lock().unwrap().redirect_tarballs_to = Some(origin.to_owned());
    }

    /// Overrides the repository size GitHub reports, in KiB.
    pub fn set_size_kb(&self, size_kb: u64) {
        self.model.lock().unwrap().size_kb = Some(size_kb);
    }

    /// The size the repository reports, in KiB: set, or its blobs' bytes rounded up.
    pub fn size_kb(&self) -> u64 {
        let model = self.model.lock().unwrap();
        size_kb(&model)
    }

    /// How many requests reached `endpoint`: `repo`, `commits`, `commits_not_modified` (the
    /// `304` answers among `commits`), `trees`, `blobs`, `tarball` or `codeload` (the
    /// download a tarball request redirects to).
    pub fn count(&self, endpoint: &str) -> usize {
        let model = self.model.lock().unwrap();
        model.counts.get(endpoint).copied().unwrap_or(0)
    }

    /// The headers of the last request received.
    pub fn last_headers(&self) -> HeaderMap {
        self.model.lock().unwrap().last_headers.clone()
    }
}

fn size_kb(model: &Model) -> u64 {
    model.size_kb.unwrap_or_else(|| {
        let bytes: u64 = model.repo.blobs.values().map(|b| b.len() as u64).sum();
        bytes.div_ceil(1024)
    })
}

#[derive(Clone)]
struct Shared {
    model: Arc<Mutex<Model>>,
    addr: SocketAddr,
}

fn json(status: StatusCode, value: serde_json::Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn not_found() -> Response {
    json(
        StatusCode::NOT_FOUND,
        serde_json::json!({ "message": "Not Found" }),
    )
}

/// Decodes `%XX` escapes in a path segment.
fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&segment[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

async fn handle(State(shared): State<Shared>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let mut model = shared.model.lock().unwrap();
    model.last_headers = headers.clone();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    if let ["_codeload", _, _, "legacy.tar.gz", commit] = segments.as_slice() {
        *model.counts.entry("codeload".into()).or_default() += 1;
        return match tarball(&model, commit) {
            Some(bytes) => (StatusCode::OK, bytes).into_response(),
            None => not_found(),
        };
    }
    let ["repos", owner, name, rest @ ..] = segments.as_slice() else {
        return not_found();
    };
    let endpoint = match rest {
        [] => "repo",
        ["commits", ..] => "commits",
        ["git", "trees", _] => "trees",
        ["git", "blobs", _] => "blobs",
        ["tarball", _] => "tarball",
        _ => return not_found(),
    };
    *model.counts.entry(endpoint.into()).or_default() += 1;

    if let Some(reset) = model.rate_limit_next.take() {
        return (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "0".to_owned()),
                ("x-ratelimit-reset", reset.to_string()),
            ],
            r#"{"message":"API rate limit exceeded"}"#,
        )
            .into_response();
    }
    if let Some(seconds) = model.retry_after_next.take() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", seconds.to_string())],
            r#"{"message":"You have exceeded a secondary rate limit"}"#,
        )
            .into_response();
    }
    let authorization = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap().to_owned());
    if let Some(message) = model.forbid_next.take() {
        return json(
            StatusCode::FORBIDDEN,
            serde_json::json!({ "message": message }),
        );
    }
    if let Some(status) = model.fail_next.take() {
        let echoed = authorization.clone().unwrap_or_default();
        return (
            StatusCode::from_u16(status).unwrap(),
            format!("something broke; you sent {echoed}"),
        )
            .into_response();
    }
    if let Some(given) = &authorization {
        let token = given.strip_prefix("Bearer ").unwrap_or_default();
        if model.token.as_deref().is_some_and(|valid| valid != token) {
            return json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "message": "Bad credentials" }),
            );
        }
    } else if model.token.is_some() {
        return not_found();
    }
    if *owner != model.owner || *name != model.name {
        return not_found();
    }

    match rest {
        [] => json(
            StatusCode::OK,
            serde_json::json!({
                "full_name": format!("{}/{}", model.owner, model.name),
                "private": model.token.is_some(),
                "size": size_kb(&model),
                "default_branch": model.repo.branches.keys().next().cloned()
                    .unwrap_or_else(|| "main".to_owned()),
                "archived": false,
            }),
        ),
        ["commits", branch @ ..] => {
            let branch = decode(&branch.join("/"));
            let Some(sha) = model.repo.branches.get(&branch).cloned() else {
                // GitHub answers 422 for a ref that names no commit.
                return json(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({ "message": format!("No commit found for SHA: {branch}") }),
                );
            };
            let etag = format!("\"{sha}\"");
            let matches = headers
                .get(header::IF_NONE_MATCH)
                .is_some_and(|given| given.to_str().unwrap() == etag);
            if matches {
                *model
                    .counts
                    .entry("commits_not_modified".into())
                    .or_default() += 1;
                return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
            }
            let tree = model.repo.commits.get(&sha).unwrap().0.clone();
            let mut response = json(
                StatusCode::OK,
                serde_json::json!({ "sha": sha, "commit": {
                    "tree": { "sha": tree },
                    "author": { "name": AUTHOR_NAME, "email": "octo@example.invalid" },
                } }),
            );
            response
                .headers_mut()
                .insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
            response
        }
        ["git", "trees", sha] => {
            let Some(rows) = model.repo.trees.get(*sha) else {
                return not_found();
            };
            let mut rows = rows.clone();
            if model.truncate_trees {
                rows.truncate(rows.len() / 2);
            }
            let tree: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    let mut value = serde_json::json!({
                        "path": row.path, "mode": row.mode, "type": row.kind, "sha": row.sha,
                    });
                    if let Some(size) = row.size {
                        value["size"] = size.into();
                    }
                    value
                })
                .collect();
            json(
                StatusCode::OK,
                serde_json::json!({ "sha": sha, "tree": tree, "truncated": model.truncate_trees }),
            )
        }
        ["git", "blobs", sha] => {
            let Some(bytes) = model.repo.blobs.get(*sha).cloned() else {
                return not_found();
            };
            let raw = headers
                .get(header::ACCEPT)
                .is_some_and(|accept| accept == "application/vnd.github.raw+json");
            if !raw {
                let content = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return json(
                    StatusCode::OK,
                    serde_json::json!({ "sha": sha, "size": bytes.len(), "encoding": "base64", "content": content }),
                );
            }
            if model.hold_open_blobs {
                let first =
                    futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from(bytes))]);
                let body = Body::from_stream(first.chain(futures_util::stream::pending()));
                return (StatusCode::OK, body).into_response();
            }
            (StatusCode::OK, bytes).into_response()
        }
        ["tarball", commit] => {
            let target = model
                .redirect_tarballs_to
                .clone()
                .unwrap_or_else(|| format!("http://{}", shared.addr));
            let location = format!(
                "{target}/_codeload/{}/{}/legacy.tar.gz/{commit}",
                model.owner, model.name
            );
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        _ => not_found(),
    }
}

/// The gzipped tarball of `commit`, under the top folder `owner-repo-<sha>/`, with a pax
/// global header first as GitHub's has.
fn tarball(model: &Model, commit: &str) -> Option<Vec<u8>> {
    let (_, entries) = model.repo.commits.get(commit)?;
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(gz);
    let top = format!("{}-{}-{commit}", model.owner, model.name);

    let pax = {
        let body = format!("comment={commit}\n");
        // The record length counts itself: two digits and a space.
        let len = body.len() + 3;
        format!("{len} {body}")
    };
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::XGlobalHeader);
    header.set_path("pax_global_header").unwrap();
    header.set_size(pax.len() as u64);
    header.set_mode(0o666);
    header.set_cksum();
    builder.append(&header, pax.as_bytes()).unwrap();

    let mut dirs = BTreeSet::new();
    for entry in entries {
        let path = match entry {
            FakeEntry::File(path, _)
            | FakeEntry::Symlink(path, _)
            | FakeEntry::Submodule(path, _) => path,
        };
        let mut parent = path.as_str();
        while let Some((dir, _)) = parent.rsplit_once('/') {
            dirs.insert(dir.to_owned());
            parent = dir;
        }
    }
    let dir_header = |name: &str, builder: &mut tar::Builder<_>| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o775);
        header.set_size(0);
        builder
            .append_data(&mut header, format!("{name}/"), std::io::empty())
            .unwrap();
    };
    dir_header(&top, &mut builder);
    for dir in &dirs {
        dir_header(&format!("{top}/{dir}"), &mut builder);
    }
    for entry in entries {
        match entry {
            FakeEntry::File(path, bytes) => {
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o664);
                header.set_size(bytes.len() as u64);
                builder
                    .append_data(&mut header, format!("{top}/{path}"), bytes.as_slice())
                    .unwrap();
            }
            FakeEntry::Symlink(path, target) => {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                builder
                    .append_link(&mut header, format!("{top}/{path}"), target)
                    .unwrap();
            }
            // GitHub's tarballs hold a submodule as an empty folder.
            FakeEntry::Submodule(path, _) => dir_header(&format!("{top}/{path}"), &mut builder),
        }
    }
    let mut gz = builder.into_inner().unwrap();
    gz.flush().unwrap();
    Some(gz.finish().unwrap())
}
