//! Reading a GitHub repository through GitHub's REST API: the repository's details, a branch's
//! head (by a conditional request, which an unchanged branch answers with `304` at no cost to
//! the rate limit), its tree, single blobs and whole tarballs.
//!
//! The API address comes from configuration only (`SHARECRAFT_GITHUB_API`). A token, when one
//! is given, goes only in the `Authorization` header: never in an address, and never in an
//! error. Redirects are followed only to the API's own host or to `codeload.github.com`,
//! where GitHub sends tarball downloads.

use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use reqwest::header::{
    ACCEPT, AUTHORIZATION, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH, RETRY_AFTER, USER_AGENT,
};
use reqwest::{StatusCode, Url, redirect};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::secrets::Secret;

/// The `User-Agent` GitHub requires: fixed, naming the product and nobody in particular.
const USER_AGENT_VALUE: &str = "ThineCraft-ShareCraft";
/// The REST API version these requests are written for.
const API_VERSION: &str = "2022-11-28";
/// Where GitHub redirects tarball downloads.
const CODELOAD_HOST: &str = "codeload.github.com";
/// The most redirects one request follows.
const MAX_REDIRECTS: usize = 5;
/// The largest JSON answer read: a tree listing of GitHub's maximum size fits well within it.
const MAX_JSON_BYTES: u64 = 64 << 20;
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a response may go without sending a byte.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Installs the ring TLS provider for this process; later calls do nothing. Every HTTPS client
/// in the process needs it, and [`GithubClient::new`] calls it.
pub fn install_tls_provider() {
    // An error means a provider is installed already, which is all that is needed.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A repository on GitHub, `owner/repo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

impl RepoRef {
    /// Parses `owner/repo`, GitHub's names only: an owner of 1 to 39 letters, digits and
    /// inner hyphens; a repository of 1 to 100 letters, digits, `.`, `-` and `_`, other than
    /// `.` and `..`. Nothing else: no further slashes, spaces, escapes or addresses.
    ///
    /// # Errors
    /// `Other` for anything else.
    pub fn parse(text: &str) -> Result<RepoRef, GithubError> {
        let bad = || {
            GithubError::Other(
                "a repository is named owner/name, for example octo-org/docs".to_owned(),
            )
        };
        let (owner, repo) = text.split_once('/').ok_or_else(bad)?;
        let owner_ok = (1..=39).contains(&owner.len())
            && owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !owner.starts_with('-')
            && !owner.ends_with('-');
        let repo_ok = (1..=100).contains(&repo.len())
            && repo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            && repo != "."
            && repo != "..";
        if owner_ok && repo_ok {
            Ok(RepoRef {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
            })
        } else {
            Err(bad())
        }
    }
}

/// What GitHub says about a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub private: bool,
    /// Its size as GitHub reports it, in KiB.
    pub size_kb: u64,
    pub default_branch: String,
    pub archived: bool,
}

/// A branch's head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// Unchanged since the ETag given.
    NotModified,
    /// The branch's commit, its root tree, and the ETag to give next time.
    Moved {
        commit: String,
        tree: String,
        etag: Option<String>,
    },
}

/// What a tree entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symbolic link (mode 120000): its blob is the link target, not content.
    Symlink,
    /// A submodule: its id is a commit in another repository.
    Submodule,
}

/// One entry of a recursive tree listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Its path from the repository root, `/`-separated.
    pub path: String,
    pub kind: EntryKind,
    /// The Git id of its blob, tree or (for a submodule) commit.
    pub sha: String,
    /// Its size in bytes for a file or symbolic link; 0 otherwise.
    pub size: u64,
}

/// Why a request to GitHub failed. Never holds a token or a response body.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GithubError {
    /// GitHub answers `404` for a private repository the token cannot see, too.
    #[error("the repository or branch was not found, or the token cannot see it")]
    NotFound,
    #[error(
        "GitHub refused the token: it is wrong, expired or revoked, or may not read this \
         repository's contents"
    )]
    Unauthorized,
    #[error("GitHub's rate limit is reached; it allows requests again at {retry_at}")]
    RateLimited { retry_at: OffsetDateTime },
    /// GitHub cut a tree listing short; a partial list must never be taken for the whole.
    #[error("the repository has too many files for GitHub to list at once")]
    Truncated,
    #[error("the download is larger than the limit allows")]
    TooLarge,
    #[error("GitHub could not be reached: {0}")]
    Network(String),
    #[error("{0}")]
    Other(String),
}

/// A redirect the client refused.
#[derive(Debug)]
struct RedirectRefused;

impl std::fmt::Display for RedirectRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("redirect to an address that is not allowed")
    }
}

impl std::error::Error for RedirectRefused {}

/// A client for GitHub's REST API at one base address.
#[derive(Clone, Debug)]
pub struct GithubClient {
    http: reqwest::Client,
    base: Url,
}

impl GithubClient {
    /// A client for the API at `base`, for example `https://api.github.com`. The caller
    /// decides whether `http://` is acceptable (configuration allows it only when set
    /// explicitly); an `https://` base makes every request and redirect HTTPS only.
    ///
    /// # Errors
    /// `Other` if `base` is not an `http(s)://` address without credentials or query, or the
    /// client cannot be set up.
    pub fn new(base: &str) -> Result<GithubClient, GithubError> {
        install_tls_provider();
        let bad = || GithubError::Other("the GitHub API address is not usable".to_owned());
        let base = Url::parse(base.trim()).map_err(|_| bad())?;
        let https = match base.scheme() {
            "https" => true,
            "http" => false,
            _ => return Err(bad()),
        };
        if base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(bad());
        }
        let home = (
            base.scheme().to_owned(),
            base.host_str().map(str::to_owned),
            base.port_or_known_default(),
        );
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(RedirectRefused);
            }
            let to = attempt.url();
            let same_home = to.scheme() == home.0
                && to.host_str() == home.1.as_deref()
                && to.port_or_known_default() == home.2;
            let codeload = to.scheme() == "https"
                && to.host_str() == Some(CODELOAD_HOST)
                && to.port_or_known_default() == Some(443);
            if same_home || codeload {
                attempt.follow()
            } else {
                attempt.error(RedirectRefused)
            }
        });
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(policy)
            .https_only(https)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| GithubError::Other("the HTTPS client could not be set up".to_owned()))?;
        Ok(GithubClient { http, base })
    }

    /// The repository's details: `GET /repos/{owner}/{repo}`.
    ///
    /// # Errors
    /// As any request; see [`GithubError`].
    pub async fn repo(&self, r: &RepoRef, token: Option<&Secret>) -> Result<RepoInfo, GithubError> {
        #[derive(Deserialize)]
        struct Answer {
            private: bool,
            size: u64,
            default_branch: String,
            #[serde(default)]
            archived: bool,
        }
        let response = self.get(self.url(r, &[])?, token, HeaderMap::new()).await?;
        let answer: Answer = read_json(response).await?;
        Ok(RepoInfo {
            private: answer.private,
            size_kb: answer.size,
            default_branch: answer.default_branch,
            archived: answer.archived,
        })
    }

    /// The head of `branch`: `GET /repos/{owner}/{repo}/commits/{branch}`, conditional on
    /// `etag` when one is given.
    ///
    /// # Errors
    /// `NotFound` for a branch that does not exist, and as any request.
    pub async fn head(
        &self,
        r: &RepoRef,
        branch: &str,
        token: Option<&Secret>,
        etag: Option<&str>,
    ) -> Result<Head, GithubError> {
        #[derive(Deserialize)]
        struct Tree {
            sha: String,
        }
        #[derive(Deserialize)]
        struct Commit {
            tree: Tree,
        }
        #[derive(Deserialize)]
        struct Answer {
            sha: String,
            commit: Commit,
        }
        if branch.is_empty() {
            return Err(GithubError::NotFound);
        }
        let mut headers = HeaderMap::new();
        if let Some(etag) = etag
            && let Ok(value) = HeaderValue::from_str(etag)
        {
            headers.insert(IF_NONE_MATCH, value);
        }
        let response = self
            .get(self.url(r, &["commits", branch])?, token, headers)
            .await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Head::NotModified);
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let answer: Answer = read_json(response).await?;
        Ok(Head::Moved {
            commit: answer.sha,
            tree: answer.commit.tree.sha,
            etag,
        })
    }

    /// Every entry under the tree `tree_sha`, recursively:
    /// `GET /repos/{owner}/{repo}/git/trees/{sha}?recursive=1`.
    ///
    /// # Errors
    /// `Truncated` when GitHub cut the listing short, and as any request.
    pub async fn tree(
        &self,
        r: &RepoRef,
        tree_sha: &str,
        token: Option<&Secret>,
    ) -> Result<Vec<TreeEntry>, GithubError> {
        #[derive(Deserialize)]
        struct Row {
            path: String,
            mode: String,
            #[serde(rename = "type")]
            kind: String,
            sha: String,
            #[serde(default)]
            size: Option<u64>,
        }
        #[derive(Deserialize)]
        struct Answer {
            tree: Vec<Row>,
            #[serde(default)]
            truncated: bool,
        }
        let mut url = self.url(r, &["git", "trees", object_id(tree_sha)?])?;
        url.query_pairs_mut().append_pair("recursive", "1");
        let response = self.get(url, token, HeaderMap::new()).await?;
        let answer: Answer = read_json(response).await?;
        if answer.truncated {
            return Err(GithubError::Truncated);
        }
        answer
            .tree
            .into_iter()
            .map(|row| {
                let kind = match (row.kind.as_str(), row.mode.as_str()) {
                    ("tree", _) => EntryKind::Dir,
                    ("commit", _) => EntryKind::Submodule,
                    ("blob", "120000") => EntryKind::Symlink,
                    ("blob", _) => EntryKind::File,
                    _ => {
                        return Err(GithubError::Other(
                            "GitHub listed a tree entry of an unknown kind".to_owned(),
                        ));
                    }
                };
                let size = match kind {
                    EntryKind::File | EntryKind::Symlink => row.size.unwrap_or(0),
                    EntryKind::Dir | EntryKind::Submodule => 0,
                };
                Ok(TreeEntry {
                    path: row.path,
                    kind,
                    sha: row.sha,
                    size,
                })
            })
            .collect()
    }

    /// The bytes of the blob `sha`, at most `limit` of them:
    /// `GET /repos/{owner}/{repo}/git/blobs/{sha}`, raw.
    ///
    /// # Errors
    /// `TooLarge` as soon as the blob is known to be over `limit` (by its declared length, or
    /// once more than `limit` bytes arrived), without reading the rest; and as any request.
    pub async fn blob(
        &self,
        r: &RepoRef,
        sha: &str,
        token: Option<&Secret>,
        limit: u64,
    ) -> Result<Vec<u8>, GithubError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github.raw+json"),
        );
        let url = self.url(r, &["git", "blobs", object_id(sha)?])?;
        let response = self.get(url, token, headers).await?;
        read_capped(response, limit).await
    }

    /// Downloads the gzipped tarball of `commit` to `dest`, at most `limit` bytes of it:
    /// `GET /repos/{owner}/{repo}/tarball/{commit}`, following GitHub's redirect to
    /// `codeload.github.com`. Read it with [`for_each_in_tarball`].
    ///
    /// # Errors
    /// `TooLarge` once the download is known to be over `limit`; `Other` if `dest` cannot be
    /// written or GitHub redirects elsewhere; and as any request. On any error `dest` is
    /// removed.
    pub async fn tarball(
        &self,
        r: &RepoRef,
        commit: &str,
        token: Option<&Secret>,
        dest: &Path,
        limit: u64,
    ) -> Result<(), GithubError> {
        let url = self.url(r, &["tarball", object_id(commit)?])?;
        let response = self.get(url, token, HeaderMap::new()).await?;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(GithubError::TooLarge);
        }
        let written = write_capped(response, dest, limit).await;
        if written.is_err() {
            let _ = tokio::fs::remove_file(dest).await;
        }
        written
    }

    /// `<base>/repos/{owner}/{repo}/<tail>`, each part escaped as one path segment (a branch
    /// `feature/x` is sent as `feature%2Fx`).
    fn url(&self, r: &RepoRef, tail: &[&str]) -> Result<Url, GithubError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| GithubError::Other("the GitHub API address is not usable".to_owned()))?
            .pop_if_empty()
            .extend(["repos", r.owner.as_str(), r.repo.as_str()])
            .extend(tail);
        Ok(url)
    }

    /// Sends a `GET` to `url` with `headers`, and `token` as a bearer token when given, and
    /// turns GitHub's refusals into errors.
    async fn get(
        &self,
        url: Url,
        token: Option<&Secret>,
        headers: HeaderMap,
    ) -> Result<reqwest::Response, GithubError> {
        let mut request = self.http.get(url).headers(headers);
        if let Some(token) = token {
            let mut value =
                HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|_| {
                    GithubError::Other("the token holds characters that cannot be sent".to_owned())
                })?;
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        let response = request.send().await.map_err(request_error)?;
        refusal(&response).map_or(Ok(response), Err)
    }
}

/// The error for an answer GitHub refused or failed with, if it did.
fn refusal(response: &reqwest::Response) -> Option<GithubError> {
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        return None;
    }
    Some(match status {
        StatusCode::UNAUTHORIZED => GithubError::Unauthorized,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            match rate_limit_reset(response.headers()) {
                Some(retry_at) => GithubError::RateLimited { retry_at },
                None if status == StatusCode::TOO_MANY_REQUESTS => GithubError::RateLimited {
                    retry_at: OffsetDateTime::now_utc() + Duration::from_secs(60),
                },
                // A token without read access to the contents is refused with 403.
                None => GithubError::Unauthorized,
            }
        }
        // GitHub answers 422 for a ref that names no commit, such as a deleted branch.
        StatusCode::NOT_FOUND | StatusCode::GONE | StatusCode::UNPROCESSABLE_ENTITY => {
            GithubError::NotFound
        }
        other => GithubError::Other(format!("GitHub answered {other}")),
    })
}

/// When a rate-limited request may be retried: from `retry-after` (seconds), or from
/// `x-ratelimit-reset` when `x-ratelimit-remaining` is 0. `None` when neither says.
fn rate_limit_reset(headers: &HeaderMap) -> Option<OffsetDateTime> {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    if let Some(seconds) = text(RETRY_AFTER.as_str()).and_then(|s| s.trim().parse::<u64>().ok()) {
        return Some(OffsetDateTime::now_utc() + Duration::from_secs(seconds));
    }
    if text("x-ratelimit-remaining").map(str::trim) == Some("0") {
        let reset = text("x-ratelimit-reset")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .and_then(|epoch| OffsetDateTime::from_unix_timestamp(epoch).ok());
        return Some(reset.unwrap_or_else(|| OffsetDateTime::now_utc() + Duration::from_secs(60)));
    }
    None
}

/// The error for a request that got no answer. Never includes the address: a download
/// address GitHub redirects to carries a token of its own.
fn request_error(error: reqwest::Error) -> GithubError {
    if error.is_redirect() {
        return GithubError::Other(
            "GitHub redirected to an address that is not allowed; nothing was read from it"
                .to_owned(),
        );
    }
    if error.is_timeout() {
        return GithubError::Network("GitHub did not answer in time".to_owned());
    }
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    GithubError::Network(text)
}

/// `id` if it is a Git object id (40 or 64 hexadecimal digits).
fn object_id(id: &str) -> Result<&str, GithubError> {
    if matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err(GithubError::Other("not a Git object id".to_owned()))
    }
}

/// The body of `response`, refused as `TooLarge` as soon as it is known to exceed `limit`.
async fn read_capped(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>, GithubError> {
    if response.content_length().is_some_and(|len| len > limit) {
        return Err(GithubError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > limit {
            return Err(GithubError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The JSON body of `response`.
async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, GithubError> {
    let body = read_capped(response, MAX_JSON_BYTES).await?;
    serde_json::from_slice(&body)
        .map_err(|_| GithubError::Other("GitHub's answer could not be read".to_owned()))
}

/// Streams the body of `response` into a new file at `dest`, at most `limit` bytes.
async fn write_capped(
    mut response: reqwest::Response,
    dest: &Path,
    limit: u64,
) -> Result<(), GithubError> {
    let write_error =
        |e: io::Error| GithubError::Other(format!("the download could not be saved: {}", e.kind()));
    let mut file = tokio::fs::File::create(dest).await.map_err(write_error)?;
    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        written = written.saturating_add(chunk.len() as u64);
        if written > limit {
            return Err(GithubError::TooLarge);
        }
        file.write_all(&chunk).await.map_err(write_error)?;
    }
    file.flush().await.map_err(write_error)?;
    file.sync_all().await.map_err(write_error)
}

/// Calls `f` with each regular file in the gzipped tarball at `path` (as
/// [`GithubClient::tarball`] saves it): its path relative to the repository root (the
/// tarball's top folder, `owner-repo-<sha>/`, taken off), its contents and its size. Folders,
/// symbolic links and other entries are skipped; the tree says what they are.
///
/// # Errors
/// Any error reading the file or from `f`; `InvalidData` for an entry outside one top folder,
/// with a `.` or `..` component, or with a path that is not UTF-8.
pub fn for_each_in_tarball(
    path: &Path,
    f: &mut dyn FnMut(&str, &mut dyn Read, u64) -> io::Result<()>,
) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(io::BufReader::new(file));
    let mut archive = tar::Archive::new(gz);
    let mut top: Option<String> = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
        let raw = entry.path_bytes();
        let text = std::str::from_utf8(&raw).map_err(|_| invalid("a path is not UTF-8"))?;
        let mut parts = text.split('/').filter(|part| !part.is_empty());
        let first = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();
        if rest.is_empty()
            || rest
                .iter()
                .chain([&first])
                .any(|p| matches!(*p, "." | ".."))
        {
            return Err(invalid("an entry is outside the repository's folder"));
        }
        match &top {
            Some(top) if top != first => {
                return Err(invalid("the tarball has more than one top folder"));
            }
            Some(_) => {}
            None => top = Some(first.to_owned()),
        }
        let relative = rest.join("/");
        let size = entry.size();
        f(&relative, &mut entry, size)?;
    }
    Ok(())
}

/// Git's id for a blob holding `data`: the SHA-1 of `blob <len>\0` and `data`, lower-case
/// hex, as GitHub's tree listing gives it. Empty in the (practically impossible) case that
/// SHA-1 collision detection fires, which matches no listed id.
#[must_use]
pub fn git_blob_id(data: &[u8]) -> String {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, data)
        .map(|id| id.to_string())
        .unwrap_or_default()
}
