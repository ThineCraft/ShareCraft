//! Reading a GitHub repository through GitHub's REST API: the repository's details, a branch's
//! head (by a conditional request, which an unchanged branch answers with `304` at no cost to
//! the rate limit), its tree, single blobs and whole tarballs.
//!
//! The API address comes from configuration only (`SHARECRAFT_GITHUB_API`). A token, when one
//! is given, goes only in the `Authorization` header: never in an address, and never in an
//! error. Redirects are followed only to the API's own host or to `codeload.github.com`,
//! where GitHub sends tarball downloads.

use std::cell::Cell;
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;
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
/// How long a whole request other than a tarball download may take, its body included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// The branch's commit, its root tree, and the ETag to give next time; `author` is the
    /// commit author's name as Git records it, when GitHub gives one (never an email).
    Moved {
        commit: String,
        tree: String,
        etag: Option<String>,
        author: Option<String>,
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
    /// `401`, or a bare `403` (one that names no rate limit), which can mean a token that
    /// lacks permission to read the repository's contents.
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
        let response = self
            .get(
                self.url(r, &[])?,
                token,
                HeaderMap::new(),
                Some(REQUEST_TIMEOUT),
            )
            .await?;
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
        struct Person {
            #[serde(default)]
            name: Option<String>,
        }
        #[derive(Deserialize)]
        struct Commit {
            tree: Tree,
            #[serde(default)]
            author: Option<Person>,
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
            .get(
                self.url(r, &["commits", branch])?,
                token,
                headers,
                Some(REQUEST_TIMEOUT),
            )
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
            author: answer.commit.author.and_then(|a| a.name),
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
        let response = self
            .get(url, token, HeaderMap::new(), Some(REQUEST_TIMEOUT))
            .await?;
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
        let response = self.get(url, token, headers, Some(REQUEST_TIMEOUT)).await?;
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
        let response = self.get(url, token, HeaderMap::new(), None).await?;
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
    /// turns GitHub's refusals into errors. `whole_request` limits the whole exchange, body
    /// included; without it only each read is limited (a tarball may take long in all).
    async fn get(
        &self,
        url: Url,
        token: Option<&Secret>,
        headers: HeaderMap,
        whole_request: Option<Duration>,
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
        if let Some(limit) = whole_request {
            request = request.timeout(limit);
        }
        let response = request.send().await.map_err(request_error)?;
        refusal(response).await
    }
}

/// `response` if GitHub answered it, or the error for its refusal or failure.
async fn refusal(response: reqwest::Response) -> Result<reqwest::Response, GithubError> {
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        return Ok(response);
    }
    Err(match status {
        StatusCode::UNAUTHORIZED => GithubError::Unauthorized,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            match rate_limit_reset(response.headers(), OffsetDateTime::now_utc()) {
                Some(retry_at) => GithubError::RateLimited { retry_at },
                None if status == StatusCode::TOO_MANY_REQUESTS => GithubError::RateLimited {
                    retry_at: in_seconds(DEFAULT_RETRY_SECONDS),
                },
                // A secondary rate limit or abuse detection may answer a bare 403 that says so
                // only in its body.
                None if mentions_secondary_limit(response).await => GithubError::RateLimited {
                    retry_at: in_seconds(DEFAULT_RETRY_SECONDS),
                },
                // A token without read access to the contents is refused with a bare 403.
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

/// How long to wait when GitHub says it is limiting requests but not until when.
const DEFAULT_RETRY_SECONDS: u64 = 60;
/// The longest `retry-after` believed, in seconds: one hour.
const MAX_RETRY_AFTER_SECONDS: u64 = 60 * 60;
/// The furthest `x-ratelimit-reset` believed, in seconds from now: one day.
const MAX_RESET_SECONDS: u64 = 24 * 60 * 60;
/// How much of a 403's body is read to look for a rate-limit message.
const MAX_REFUSAL_BODY: usize = 4096;

/// Now plus `seconds`, or now if that cannot be represented.
fn in_seconds(seconds: u64) -> OffsetDateTime {
    after(OffsetDateTime::now_utc(), seconds)
}

/// `from` plus `seconds`, or `from` if that cannot be represented.
fn after(from: OffsetDateTime, seconds: u64) -> OffsetDateTime {
    i64::try_from(seconds)
        .ok()
        .and_then(|s| from.checked_add(time::Duration::seconds(s)))
        .unwrap_or(from)
}

/// Whether the first [`MAX_REFUSAL_BODY`] bytes of a refusal's body mention a secondary rate
/// limit or abuse detection. The body itself is never kept or shown.
async fn mentions_secondary_limit(mut response: reqwest::Response) -> bool {
    let mut body = Vec::new();
    while body.len() < MAX_REFUSAL_BODY {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_REFUSAL_BODY.saturating_sub(body.len());
                body.extend(chunk.iter().take(room));
            }
            Ok(None) | Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&body).to_lowercase();
    text.contains("secondary rate limit") || text.contains("abuse")
}

/// When a rate-limited request may be retried, judged at `now`: from `retry-after` (seconds,
/// at most an hour), or from `x-ratelimit-reset` when `x-ratelimit-remaining` is 0 (at most a
/// day ahead). `None` when neither says.
fn rate_limit_reset(headers: &HeaderMap, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    if let Some(seconds) = text(RETRY_AFTER.as_str()).and_then(|s| s.trim().parse::<u64>().ok()) {
        return Some(after(now, seconds.min(MAX_RETRY_AFTER_SECONDS)));
    }
    if text("x-ratelimit-remaining").map(str::trim) == Some("0") {
        let latest = after(now, MAX_RESET_SECONDS);
        let reset = text("x-ratelimit-reset")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|epoch| match OffsetDateTime::from_unix_timestamp(epoch) {
                Ok(reset) => reset.min(latest),
                // Out of range: so far ahead that the cap applies.
                Err(_) if epoch > 0 => latest,
                Err(_) => now,
            });
        return Some(reset.unwrap_or_else(|| after(now, DEFAULT_RETRY_SECONDS)));
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

/// Caps on what reading a tarball may expand to, checked against each entry's header before
/// its contents are decoded, so a small download cannot expand without bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TarLimits {
    /// The largest single entry, in bytes.
    pub max_entry_bytes: u64,
    /// The most bytes all entries together may hold.
    pub max_total_bytes: u64,
    /// The most entries of any kind (files, folders, links, headers).
    pub max_entries: usize,
}

/// Calls `f` with each regular file in the gzipped tarball at `path` (as
/// [`GithubClient::tarball`] saves it): its path relative to the repository root (the
/// tarball's top folder, `owner-repo-<sha>/`, taken off), its contents and its size. Folders,
/// symbolic links and other entries are skipped; the tree says what they are.
///
/// The paths given to `f` are repository keys (`/`-separated names, each a plain name), never
/// paths on disk; never join one to a folder to write a file.
///
/// # Errors
/// Any error reading the file or from `f`. `InvalidData` for a path that is not UTF-8, starts
/// with `/`, holds a backslash, a `:` (a drive or stream), an empty, `.` or `..` component, or
/// lies outside one top folder, or holds a control character. `FileTooLarge` when an entry,
/// the total, or the number of entries passes `limits`, or when the decompressed stream
/// (headers and padding included) passes the total with room for headers. Nothing is passed to `f` after the first error.
pub fn for_each_in_tarball(
    path: &Path,
    limits: &TarLimits,
    f: &mut dyn FnMut(&str, &mut dyn Read, u64) -> io::Result<()>,
) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(io::BufReader::new(file));
    let tripped = Rc::new(Cell::new(false));
    let capped = Capped {
        inner: gz,
        left: decompressed_cap(limits),
        tripped: tripped.clone(),
    };
    let result = read_entries(tar::Archive::new(capped), limits, f);
    // Whatever error the cap surfaced as (tar may wrap it), it is reported as the cap.
    if result.is_err() && tripped.get() {
        return Err(too_large(TOO_MUCH_EXPANSION));
    }
    result
}

/// The fixed message for a tarball that expands past its limits.
const TOO_MUCH_EXPANSION: &str = "the tarball expands to more than the limit allows";
/// Room allowed per entry, beyond its contents, for its header, extension headers (a long
/// name or pax record) and padding.
const PER_ENTRY_OVERHEAD: u64 = 4096;
/// Room allowed once, beyond everything else, for the end of the archive.
const ARCHIVE_OVERHEAD: u64 = 1 << 20;

/// The most decompressed bytes a tarball within `limits` can hold, with room for headers.
fn decompressed_cap(limits: &TarLimits) -> u64 {
    let entries = u64::try_from(limits.max_entries).unwrap_or(u64::MAX);
    limits
        .max_total_bytes
        .saturating_add(entries.saturating_mul(PER_ENTRY_OVERHEAD))
        .saturating_add(ARCHIVE_OVERHEAD)
}

fn too_large(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, why.to_owned())
}

/// A reader that fails once more than `left` bytes have come through it, so nothing read
/// from it (a header, an extension header tar reads whole into memory, padding or a skipped
/// entry) can expand past the cap. Records in `tripped` that it failed.
struct Capped<R> {
    inner: R,
    left: u64,
    tripped: Rc<Cell<bool>>,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.tripped.get() {
            return Err(too_large(TOO_MUCH_EXPANSION));
        }
        let n = self.inner.read(buf)?;
        match self.left.checked_sub(n as u64) {
            Some(left) => {
                self.left = left;
                Ok(n)
            }
            None => {
                self.tripped.set(true);
                Err(too_large(TOO_MUCH_EXPANSION))
            }
        }
    }
}

/// The loop of [`for_each_in_tarball`] over `archive`.
fn read_entries<R: Read>(
    mut archive: tar::Archive<R>,
    limits: &TarLimits,
    f: &mut dyn FnMut(&str, &mut dyn Read, u64) -> io::Result<()>,
) -> io::Result<()> {
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
    let mut top: Option<String> = None;
    let mut entries: usize = 0;
    let mut total: u64 = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entries = entries.saturating_add(1);
        if entries > limits.max_entries {
            return Err(too_large(
                "the tarball has more entries than the limit allows",
            ));
        }
        let size = entry.size();
        if size > limits.max_entry_bytes {
            return Err(too_large(
                "a file in the tarball is larger than the limit allows",
            ));
        }
        total = total.saturating_add(size);
        if total > limits.max_total_bytes {
            return Err(too_large(TOO_MUCH_EXPANSION));
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let relative = {
            let raw = entry.path_bytes();
            let text = std::str::from_utf8(&raw).map_err(|_| invalid("a path is not UTF-8"))?;
            let (first, rest) = repository_path(text)
                .ok_or_else(|| invalid("an entry's path is not a plain path in the repository"))?;
            match &top {
                Some(top) if top != first => {
                    return Err(invalid("the tarball has more than one top folder"));
                }
                Some(_) => {}
                None => top = Some(first.to_owned()),
            }
            rest.to_owned()
        };
        f(&relative, &mut entry, size)?;
    }
    Ok(())
}

/// Splits a tarball entry's path into its top folder and the path below it, if every
/// component is a plain name: no leading `/`, no backslash, no `:`, and no empty, `.` or `..`
/// component. A path with nothing below the top folder is refused too.
fn repository_path(text: &str) -> Option<(&str, &str)> {
    if text.starts_with('/') || text.contains('\\') {
        return None;
    }
    let plain = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !part.contains(':')
            && !part.bytes().any(|b| b.is_ascii_control())
    };
    if !text.split('/').all(plain) {
        return None;
    }
    text.split_once('/')
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
