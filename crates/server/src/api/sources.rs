//! Connecting a library to a branch of a GitHub repository, a file-share folder or an S3
//! bucket, and managing the connection: connect (a new library, made only once the source
//! answers that it is there and usable), show, change, "Sync now", the webhook secret and
//! disconnect.
//!
//! Secrets (the token, the webhook secret) are sealed (see [`crate::secrets`]) before they
//! reach the database, and no answer, event, log line or error carries one: the only exception
//! is the webhook secret in the one answer that generates it.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::{
    AccessError, Budget, LibraryKind, NewEvent, NewSource, Perms, S3Endpoint, S3Place, SealedRow,
    SecretId, SharePath, SourceChange, SourceConfig, SourceKind, SourceMode, SourceStatus, Tri,
    Usage, User, Versioning, WriteState, approved_root, clean_prefix, s3_location, usable_here,
    valid_bucket, valid_root,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use uuid::Uuid;

use super::nodes::{node_id, require_manager, rfc3339};
use super::settings::{SHARE_TURN_WAIT, overlaps_data_folder, path_reason};
use super::{ApiError, JsonBody, recheck_below, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::github::{EntryKind, GithubError, Head, RepoRef};
use crate::libraries;
use crate::s3::{S3Error, S3Target};
use crate::s3_sign::Credentials;
use crate::s3_write;
use crate::secrets::Secret;
use crate::share::{FileIdentity, FsBudget, canonical_share_path};
use crate::source_sync::{abandon_uploads_into, caps, token_context, token_of, webhook_context};
use crate::source_write::{self, Probe};
use crate::tree::{self, TreeOp};

/// Seconds between polls of a new connection.
const DEFAULT_POLL_SECONDS: u32 = 300;
/// The longest branch name taken, in bytes.
const MAX_BRANCH_BYTES: usize = 200;
/// The longest folder taken, in bytes.
const MAX_FOLDER_BYTES: usize = 500;
/// The longest token taken, in bytes.
const MAX_TOKEN_BYTES: usize = 1000;
/// The bytes of a webhook secret.
const WEBHOOK_SECRET_BYTES: usize = 32;
/// The longest S3 access key id taken, in bytes.
const MAX_KEY_ID_BYTES: usize = 128;
/// The longest S3 secret access key taken, in bytes.
const MAX_SECRET_KEY_BYTES: usize = 1000;
/// The longest the probe of a bucket may take, both of its requests together.
const S3_PROBE_LIMIT: Duration = Duration::from_secs(20);

/// Asks the sync worker to sync `library` at once, after an owner acted on its connection: its
/// failures in a row are forgotten (no back-off left from before). The outcome is recorded in
/// the connection's status and the event log, so nothing waits for it, and nothing about it
/// can fail the request that asked for it. A sync already running (which read the connection
/// before the change, and so records nothing of its result) is followed by another.
fn wake(state: &AppState, library: NodeId) {
    state.sources().forget_failures(library);
    state.sources().wake(library);
}

/// Whether `branch` is a branch name Git takes: not empty, `.` or `..`; no `..`, `@{`,
/// backslash or control character; not starting with `-` or `/`, not ending with `/` or
/// `.lock`; at most 200 bytes.
fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= MAX_BRANCH_BYTES
        && branch != "."
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.contains('\\')
        && !branch.chars().any(char::is_control)
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.ends_with(".lock")
}

/// Whether `folder` is empty (the whole repository) or a relative path of plain names
/// separated by `/`: no empty, `.` or `..` name, no backslash, `:` or control character; at
/// most 500 bytes.
fn valid_folder(folder: &str) -> bool {
    folder.is_empty()
        || (folder.len() <= MAX_FOLDER_BYTES
            && !folder.contains('\\')
            && !folder.contains(':')
            && !folder.chars().any(char::is_control)
            && folder
                .split('/')
                .all(|name| !name.is_empty() && name != "." && name != ".."))
}

/// `token` if it can be one: 1 to 1000 visible ASCII characters (it travels in a header).
fn checked_token(token: String) -> Result<Secret, ApiError> {
    let ok = !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token.bytes().all(|b| b.is_ascii_graphic());
    if ok {
        Ok(Secret::new(token))
    } else {
        Err(ApiError::InvalidRequest)
    }
}

/// Which request a GitHub failure answered, so a `404` names what was not found.
#[derive(Clone, Copy)]
enum Asked {
    Repository,
    Branch,
    Tree,
}

/// The answer for a GitHub failure while probing. Never GitHub's text or the token.
fn refused(asked: Asked, e: GithubError) -> ApiError {
    match (e, asked) {
        (GithubError::NotFound, Asked::Repository) => ApiError::RepositoryNotFound,
        (GithubError::NotFound, Asked::Branch) => ApiError::BranchNotFound,
        (GithubError::Unauthorized, _) => ApiError::TokenRejected,
        (e, _) => {
            // `GithubError` holds no token or response body.
            eprintln!("sharecraft-server: GitHub could not be asked about a repository: {e}");
            ApiError::Busy
        }
    }
}

/// What the probe found.
struct Probed {
    branch: String,
    archived: bool,
}

/// Asks GitHub whether `repo` is there and visible (with `token`, if given), whether `branch`
/// (the repository's default when `None`) is there, and whether the repository is within the
/// caps: its size as GitHub reports it against `source_max_bytes`, and its files below
/// `folder` against `source_max_files`. A tree too large for GitHub to list in one answer is
/// left for the sync to judge.
async fn probe(
    state: &AppState,
    repo: &RepoRef,
    branch: Option<&str>,
    folder: &str,
    token: Option<&Secret>,
) -> Result<Probed, ApiError> {
    let github = state.github();
    let info = github
        .repo(repo, token)
        .await
        .map_err(|e| refused(Asked::Repository, e))?;
    let branch = branch.unwrap_or(&info.default_branch).to_owned();
    if !valid_branch(&branch) {
        return Err(ApiError::BranchNotFound);
    }
    let (max_files, max_bytes) = caps(state).await.map_err(|_| ApiError::Unavailable)?;
    if info.size_kb > max_bytes / 1024 {
        return Err(ApiError::RepositoryTooLarge);
    }
    let head = github
        .head(repo, &branch, token, None)
        .await
        .map_err(|e| refused(Asked::Branch, e))?;
    let Head::Moved { tree, .. } = head else {
        // Never answered to a request without an entity tag.
        return Err(ApiError::Busy);
    };
    match github.tree(repo, &tree, token).await {
        Ok(entries) => {
            let folder = folder.trim_matches('/');
            let files = entries
                .iter()
                .filter(|e| e.kind == EntryKind::File)
                .filter(|e| {
                    folder.is_empty()
                        || e.path
                            .strip_prefix(folder)
                            .is_some_and(|rest| rest.starts_with('/'))
                })
                .count();
            if files > max_files {
                return Err(ApiError::RepositoryTooLarge);
            }
        }
        Err(GithubError::Truncated) => {}
        Err(e) => return Err(refused(Asked::Tree, e)),
    }
    Ok(Probed {
        branch,
        archived: info.archived,
    })
}

/// Seals `plain` for `context` and stores it; its id.
async fn store_secret(
    state: &AppState,
    plain: &[u8],
    context: &[u8],
) -> Result<SecretId, ApiError> {
    let sealed = state.secrets().seal(plain, context).map_err(|e| {
        eprintln!("sharecraft-server: a secret could not be sealed: {e}");
        ApiError::Unavailable
    })?;
    state
        .db()
        .put_secret(&SealedRow {
            key_id: sealed.key_id,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        })
        .await
        .map_err(|e| ApiError::unavailable(&e))
}

/// Deletes the secret `id`; a failure is logged (the row holds only ciphertext).
async fn forget_secret(state: &AppState, id: SecretId) {
    if let Err(e) = state.db().delete_secret(id).await {
        eprintln!("sharecraft-server: a sealed secret could not be deleted: {e}");
    }
}

/// The origin pages are served from, for the webhook's address (see `links::create`).
fn origin(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    // The API's guard has checked that `Host` names this machine; production mode ignores it
    // and names the public address.
    Ok(match state.deployment().production() {
        Some(production) => production.public.origin().to_owned(),
        None => state.deployment().link_origin(
            headers
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .ok_or(ApiError::Forbidden)?,
        ),
    })
}

/// The address GitHub sends the webhook of `library` to.
fn webhook_url(origin: &str, library: NodeId) -> String {
    format!("{origin}/hooks/github/{}", library.0)
}

/// A connection as the routes answer it. Never a secret, and never the sync bookmark's entity
/// tag; the commit is cut to seven characters.
fn source_out(
    origin: &str,
    config: &SourceConfig,
    status: &SourceStatus,
    reveal_path: bool,
) -> Value {
    if config.kind == SourceKind::Share {
        return json!({
            "kind": "share",
            "mode": config.mode.as_str(),
            "path": if reveal_path { config.location.clone() } else { None },
            "state": status.state.as_str(),
            "last_synced_at": rfc3339(status.last_synced_at),
            "last_checked_at": rfc3339(status.last_checked_at),
            "last_error": status.last_error,
            "files": status.files,
            "bytes": status.bytes,
            "hidden": status.hidden,
            "hidden_by": status.hidden_detail,
            "poll_seconds": config.poll_seconds,
            "next_check_at": rfc3339(status.next_check_at),
        });
    }
    json!({
        "kind": config.kind.as_str(),
        "repository": format!("{}/{}", config.owner, config.repo),
        "branch": config.branch,
        "folder": config.folder,
        "state": status.state.as_str(),
        "last_commit": status.last_commit.as_deref().map(|c| c.get(..7).unwrap_or(c)),
        "last_synced_at": rfc3339(status.last_synced_at),
        "last_checked_at": rfc3339(status.last_checked_at),
        "last_error": status.last_error,
        "files": status.files,
        "bytes": status.bytes,
        "hidden": status.hidden,
        "poll_seconds": config.poll_seconds,
        "token_set": config.token.is_some(),
        "webhook": {
            "url": webhook_url(origin, config.library),
            "configured": config.webhook_secret.is_some(),
        },
    })
}

/// The connection of `library`; `404` if it has none.
async fn source_of(
    state: &AppState,
    library: NodeId,
) -> Result<(SourceConfig, SourceStatus), ApiError> {
    state
        .db()
        .source_of(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)
}

/// The connection of `library` as answered, read afresh.
async fn answer_of(
    state: &AppState,
    origin: &str,
    library: NodeId,
    reveal_path: bool,
    admin: bool,
) -> Result<Value, ApiError> {
    let (config, status) = source_of(state, library).await?;
    if config.kind == SourceKind::S3 {
        return s3_answer(state, &config, &status, reveal_path, admin).await;
    }
    let mut answer = source_out(origin, &config, &status, reveal_path);
    if config.kind == SourceKind::Share
        && status.state == records::SourceState::Syncing
        && let Some(files) = state.shares().listing_progress(library)
        && let Some(object) = answer.as_object_mut()
    {
        object.insert("listing_now".to_owned(), json!({ "files_so_far": files }));
    }
    Ok(answer)
}

/// Records `kind` about `library`, done by `actor`.
fn emit(state: &AppState, kind: &'static str, actor: &User, library: NodeId, detail: Value) {
    state.events().emit(NewEvent {
        kind,
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: Some(library),
        detail,
    });
}

/// The body of `POST /api/sites/{id}/libraries/github`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConnectBody {
    name: String,
    /// `owner/repo`.
    repository: String,
    /// The repository's default branch when not given.
    branch: Option<String>,
    /// The whole repository when not given.
    folder: Option<String>,
    /// For a private repository: a read-only token.
    token: Option<String>,
    /// `document` when not given.
    kind: Option<String>,
}

/// `POST /api/sites/{id}/libraries/github {name, repository, branch?, folder?, token?,
/// kind?}`: makes a new library in the site, connected to the branch, for someone who may make
/// a library there (`manage_site`; an `asset` library needs an administrator too), and starts
/// its first sync in the background. GitHub is asked first: `422 repository_not_found`,
/// `token_rejected`, `branch_not_found` or `repository_too_large` (and `503 busy` when GitHub
/// cannot be asked) make nothing. `422 invalid_request` for a repository not named `owner/repo`
/// as GitHub names them, a branch Git would refuse, a folder that is not a relative path of
/// plain names, or an empty token. `201 {id, source, warnings}`, `warnings` holding `archived`
/// for an archived repository.
pub(crate) async fn connect(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ConnectBody>,
) -> Result<Response, ApiError> {
    let site = node_id(&id)?;
    let kind = match body.kind.as_deref() {
        None => LibraryKind::Document,
        Some(word) => libraries::parse_kind(word)?,
    };
    let repo = RepoRef::parse(&body.repository).map_err(|_| ApiError::InvalidRequest)?;
    if body.branch.as_deref().is_some_and(|b| !valid_branch(b)) {
        return Err(ApiError::InvalidRequest);
    }
    let folder = body.folder.unwrap_or_default();
    if !valid_folder(&folder) {
        return Err(ApiError::InvalidRequest);
    }
    let token = body.token.map(checked_token).transpose()?;
    let name = records::validate_node_name(&body.name)
        .map_err(ApiError::from)?
        .to_owned();
    let origin = origin(&state, &headers)?;
    let actor = signed.session.user;

    // Who may make a library here, judged before GitHub is asked anything.
    let (place, access) = tree::visible(state.db(), &actor, site).await?;
    if place.kind != records::NodeKind::Site {
        return Err(ApiError::WrongPlace);
    }
    tree::require(&access, Perms::MANAGE_SITE)?;
    if kind == LibraryKind::Asset && !actor.is_admin {
        return Err(ApiError::Forbidden);
    }
    // A taken name costs no request to GitHub; `create_library` checks it again.
    if !state
        .db()
        .name_free(site, &name, None)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::NameTaken);
    }

    let probed = probe(
        &state,
        &repo,
        body.branch.as_deref(),
        &folder,
        token.as_ref(),
    )
    .await?;
    let library = NodeId(Uuid::new_v4());
    let detail = json!({
        "repository": format!("{}/{}", repo.owner, repo.repo),
        "branch": probed.branch,
        "folder": folder,
        "token_set": token.is_some(),
    });
    {
        let state = state.clone();
        let actor = actor.clone();
        let branch = probed.branch.clone();
        to_completion(async move {
            let token = match token {
                Some(token) => Some(
                    store_secret(&state, token.expose().as_bytes(), &token_context(library))
                        .await?,
                ),
                None => None,
            };
            let made = tree::create_library(&state, &actor, site, library, &name, kind).await;
            if let Err(e) = made {
                if let Some(id) = token {
                    forget_secret(&state, id).await;
                }
                return Err(e.into());
            }
            let connected = state
                .db()
                .connect_source(&NewSource {
                    library,
                    kind: SourceKind::Github,
                    owner: repo.owner,
                    repo: repo.repo,
                    branch,
                    folder,
                    location: None,
                    mode: SourceMode::ReadOnly,
                    token,
                    poll_seconds: DEFAULT_POLL_SECONDS,
                    connected_by: actor.id,
                })
                .await;
            if let Err(e) = connected {
                purge_new_library(&state, &actor, library).await;
                if let Some(id) = token {
                    forget_secret(&state, id).await;
                }
                return Err(e.into());
            }
            abandon_uploads_into(&state, library).await;
            emit(&state, "source.connected", &actor, library, detail);
            wake(&state, library);
            Ok(())
        })
        .await?;
    }
    let source = answer_of(&state, &origin, library, true, actor.is_admin).await?;
    let warnings: Vec<&str> = if probed.archived {
        vec!["archived"]
    } else {
        Vec::new()
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": library.0.to_string(),
            "source": source,
            "warnings": warnings,
        })),
    )
        .into_response())
}

/// The body of `POST /api/sites/{id}/libraries/share`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConnectShareBody {
    name: String,
    path: String,
    kind: Option<String>,
    mode: Option<String>,
}

fn share_probe_error(error: &io::Error) -> ApiError {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
            ApiError::ShareUnreachable("not_found")
        }
        io::ErrorKind::PermissionDenied => ApiError::ShareUnreachable("denied"),
        _ => ApiError::ShareUnreachable("offline"),
    }
}

/// How many times a share connect resolves locations connected while it was resolving before
/// it gives up with a retryable `503`.
const SHARE_RESOLVE_ROUNDS: usize = 3;

/// Why a canonical lookup gave no location.
enum Lookup {
    /// No lookup permit came free before the deadline: the server already has
    /// [`crate::share::SHARE_LOOKUPS`] lookups in flight, most likely blocked on a hung server.
    Busy,
    /// The lookup failed or did not finish by the deadline.
    Failed(ApiError),
}

/// Where a connected or new share folder really is.
#[derive(Clone)]
struct Resolved {
    /// Its canonical location.
    canonical: SharePath,
    /// Its own file-system identity, when the platform gives one.
    own: Option<FileIdentity>,
    /// The identities of the folder and of each folder above it up to its approved root, those
    /// the platform gives: a folder whose identity is here is the folder or one around it.
    chain: Vec<FileIdentity>,
}

impl Resolved {
    /// Whether, by file-system identity, one of `self` and `other` is the other or inside it,
    /// whatever names reach them (a case-insensitive mount, another alias of one folder).
    /// `false` when either identity is unknown: the path comparisons then decide alone.
    fn aliases(&self, other: &Resolved) -> bool {
        self.own.is_some_and(|own| other.chain.contains(&own))
            || other.own.is_some_and(|own| self.chain.contains(&own))
    }
}

/// The canonical location of `path` within `root`, through the contained [`crate::share::ShareFs`] seam,
/// waiting no later than `deadline`, with the file-system identities of the folder and the
/// folders above it within `root` (an identity that cannot be read is left out). The blocking
/// call holds one of the server's lookup permits until it returns, even after this caller stops
/// waiting for it. A failure never carries the canonical path or operating-system text into an
/// answer or a log.
async fn resolve_canonical(
    state: &AppState,
    root: &SharePath,
    path: &SharePath,
    deadline: tokio::time::Instant,
) -> Result<Resolved, Lookup> {
    let permit = tokio::time::timeout_at(deadline, state.shares().lookups().acquire_owned())
        .await
        .map_err(|_| Lookup::Busy)?
        .map_err(|_| Lookup::Busy)?;
    let fs = state.shares().fs();
    let root = root.to_path_buf();
    let candidate = path.to_path_buf();
    let (resolved, own, chain) = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            let resolved = fs.canonical(&root, &candidate);
            let mut own = None;
            let mut chain = Vec::new();
            if resolved.is_ok() {
                // The folder, then each folder above it, as far as the approved root.
                let depth = candidate
                    .components()
                    .count()
                    .saturating_sub(root.components().count());
                for (step, folder) in candidate
                    .ancestors()
                    .take(depth.saturating_add(1))
                    .enumerate()
                {
                    if let Ok(Some(identity)) = fs.identity(&root, folder) {
                        if step == 0 {
                            own = Some(identity);
                        }
                        chain.push(identity);
                    }
                }
            }
            drop(permit);
            resolved.map(|resolved| (resolved, own, chain))
        }),
    )
    .await
    .map_err(|_| Lookup::Failed(ApiError::ShareUnreachable("offline")))?
    .map_err(|_| Lookup::Failed(ApiError::ShareUnreachable("offline")))?
    .map_err(|error| Lookup::Failed(share_probe_error(&error)))?;
    let canonical = canonical_share_path(&resolved)
        .map_err(|_| Lookup::Failed(ApiError::ShareUnreachable("offline")))?;
    Ok(Resolved {
        canonical,
        own,
        chain,
    })
}

/// Makes a new read-only library backed by an administrator-approved file-share folder.
pub(crate) async fn connect_share(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ConnectShareBody>,
) -> Result<Response, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;

    // Access is judged before the path is parsed or the file system is touched.
    let (place, access) = tree::visible(state.db(), &actor, site).await?;
    if place.kind != records::NodeKind::Site {
        return Err(ApiError::WrongPlace);
    }
    tree::require(&access, Perms::MANAGE_SITE)?;

    let kind = match body.kind.as_deref() {
        None | Some("document") => LibraryKind::Document,
        Some("image") => LibraryKind::Image,
        Some(_) => return Err(ApiError::InvalidRequest),
    };
    if body.mode.as_deref().is_some_and(|mode| mode != "read_only") {
        return Err(ApiError::InvalidRequest);
    }
    let name = records::validate_node_name(&body.name)
        .map_err(ApiError::from)?
        .to_owned();
    let path = SharePath::parse(&body.path)
        .map_err(|reason| ApiError::InvalidSharePath(path_reason(reason)))?;
    valid_root(&path)
        .and_then(|()| usable_here(&path))
        .map_err(|reason| ApiError::InvalidSharePath(path_reason(reason)))?;
    let settings = state
        .db()
        .share_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let mut approved = approved_root(&path, &settings.roots)
        .cloned()
        .ok_or(ApiError::ShareNotApproved)?;
    let data_deadline = tokio::time::Instant::now() + Duration::from_secs(settings.list_seconds);
    if overlaps_data_folder(&state, &path, data_deadline).await? {
        return Err(ApiError::ShareNotApproved);
    }
    if !state
        .db()
        .name_free(site, &name, None)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::NameTaken);
    }

    // Probe only the connected folder itself; recursive walking belongs to the first listing.
    // Like a canonical lookup, the probe holds one of the server's lookup permits for its whole
    // blocking call and is waited for no longer than `list_seconds`.
    let probe_fs = state.shares().fs();
    let approved_path = approved.to_path_buf();
    let share_path = path.to_path_buf();
    let max_files = settings.max_files;
    let max_time = Duration::from_secs(settings.list_seconds);
    let probe_deadline = tokio::time::Instant::now() + max_time;
    let permit = tokio::time::timeout_at(probe_deadline, state.shares().lookups().acquire_owned())
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Unavailable)?;
    tokio::time::timeout_at(
        probe_deadline,
        tokio::task::spawn_blocking(move || {
            let listed = probe_fs.list(
                &approved_path,
                &share_path,
                &FsBudget::new(max_files, Instant::now() + max_time),
            );
            drop(permit);
            listed
        }),
    )
    .await
    .map_err(|_| ApiError::ShareUnreachable("offline"))?
    .map_err(|_| ApiError::ShareUnreachable("offline"))?
    .map_err(|error| share_probe_error(&error))?;

    // Every file-system call happens outside the organisation-wide connection turn, so the
    // turn never waits on share I/O. The new folder's canonical location and that of every
    // share already connected are resolved at once, under one `list_seconds` deadline; an
    // existing location that cannot be resolved in time falls back to its typed path, but when
    // no lookup permit comes free in time the answer is a retryable `503` instead. Under
    // the turn only the database is touched: the roots and the connected locations are read
    // again and compared with what was resolved. A location connected meanwhile was never
    // resolved, so the turn is given up, just the new locations are resolved (with a fresh
    // deadline), and the comparison is repeated; after a few rounds of that the answer is a
    // retryable `503`. So is a root change that moves the folder under a different approved root
    // meanwhile: its own location is then resolved again within the new root.
    let mut resolved: HashMap<String, Option<Resolved>> = HashMap::new();
    let mut own_canonical: Option<Resolved> = None;
    let mut held = None;
    for _ in 0..SHARE_RESOLVE_ROUNDS {
        let deadline = tokio::time::Instant::now() + max_time;
        let unresolved: Vec<String> = state
            .db()
            .share_locations()
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .into_iter()
            .filter(|location| !resolved.contains_key(location))
            .collect();
        let resolving_existing =
            futures_util::future::join_all(unresolved.into_iter().map(|location| {
                let state = &state;
                let roots = settings.roots.clone();
                async move {
                    let canonical = match SharePath::parse(&location) {
                        Ok(lexical) => match approved_root(&lexical, &roots) {
                            Some(root) => {
                                match resolve_canonical(state, root, &lexical, deadline).await {
                                    Ok(canonical) => Ok(Some(canonical)),
                                    // Too many lookups are already stuck: answer a
                                    // retryable 503 rather than skip this location's
                                    // canonical comparison.
                                    Err(Lookup::Busy) => Err(ApiError::Unavailable),
                                    Err(Lookup::Failed(_)) => Ok(None),
                                }
                            }
                            None => Ok(None),
                        },
                        Err(_) => Ok(None),
                    };
                    (location, canonical)
                }
            }));
        let newly = if own_canonical.is_none() {
            let (own, newly) = tokio::join!(
                resolve_canonical(&state, &approved, &path, deadline),
                resolving_existing
            );
            own_canonical = Some(own.map_err(|lookup| match lookup {
                Lookup::Busy => ApiError::Unavailable,
                Lookup::Failed(error) => error,
            })?);
            newly
        } else {
            resolving_existing.await
        };
        for (location, canonical) in newly {
            resolved.insert(location, canonical?);
        }
        let Some(own) = own_canonical.clone() else {
            return Err(ApiError::ShareUnreachable("offline"));
        };
        let canonical_path = &own.canonical;

        // The wait for the turn is bounded; a timeout is a retryable `503`. The turn is then
        // held through the source insert, so a concurrent root change or connect linearises
        // wholly before or after this commit.
        let mut turn = state
            .db()
            .lock_share_connections(SHARE_TURN_WAIT)
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .ok_or(ApiError::Unavailable)?;
        let current_settings = state
            .db()
            .share_settings()
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        let Some(current_root) = approved_root(&path, &current_settings.roots).cloned() else {
            return Err(ApiError::ShareNotApproved);
        };
        if current_root != approved {
            // The folder's own canonical location was judged within a root that has since
            // been replaced: give the turn up and resolve it again within the current root.
            approved = current_root;
            own_canonical = None;
            continue;
        }
        let existing = state
            .db()
            .share_locations_locked(&mut turn, NodeId(Uuid::nil()))
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        let mut complete = true;
        for location in existing {
            let lexical = SharePath::parse(&location).map_err(|_| ApiError::Unavailable)?;
            let lexical_conflict = path.is_within(&lexical) || lexical.is_within(&path);
            let conflict = match resolved.get(&location) {
                Some(Some(existing)) => {
                    canonical_path.is_within(&existing.canonical)
                        || existing.canonical.is_within(canonical_path)
                        || own.aliases(existing)
                }
                Some(None) => lexical_conflict,
                None => {
                    complete = false;
                    lexical_conflict
                }
            };
            if conflict {
                return Err(ApiError::ShareLocationInUse);
            }
        }
        if complete {
            held = Some(turn);
            break;
        }
    }
    let mut connection_turn = held.ok_or(ApiError::Unavailable)?;

    let library = NodeId(Uuid::new_v4());
    let location = path.display();
    let origin = origin(&state, &headers)?;
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            tree::create_library(&state, &actor, site, library, &name, kind).await?;
            if let Err(error) = state
                .db()
                .connect_source_locked(
                    &mut connection_turn,
                    &NewSource {
                        library,
                        kind: SourceKind::Share,
                        owner: String::new(),
                        repo: String::new(),
                        branch: String::new(),
                        folder: String::new(),
                        location: Some(location),
                        mode: SourceMode::ReadOnly,
                        token: None,
                        poll_seconds: DEFAULT_POLL_SECONDS,
                        connected_by: actor.id,
                    },
                )
                .await
            {
                purge_new_library(&state, &actor, library).await;
                return Err(error.into());
            }
            abandon_uploads_into(&state, library).await;
            emit(
                &state,
                "source.connected",
                &actor,
                library,
                json!({ "kind": "share" }),
            );
            wake(&state, library);
            Ok(())
        })
        .await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": library.0.to_string(),
            "source": answer_of(&state, &origin, library, true, actor.is_admin).await?,
        })),
    )
        .into_response())
}

/// Removes `library`, made a moment ago by `actor` for a connection that then failed. It is
/// deleted first, closing the create race. Only a library that still has no children is then
/// purged; if concurrent user content reached it, the library and that content stay recoverable
/// in the recycle bin. A failure is logged.
async fn purge_new_library(state: &AppState, actor: &User, library: NodeId) {
    if let Err(e) = tree::apply(state, actor, TreeOp::Delete { id: library }).await {
        eprintln!(
            "sharecraft-server: library {} could not be removed after its connection failed: {e}",
            library.0
        );
        return;
    }
    match state.db().has_any_children(library).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!(
                "sharecraft-server: library {} stayed in the recycle bin after its connection \
                 failed because its contents could not be checked: {e}",
                library.0
            );
            return;
        }
    }
    for op in [
        TreeOp::ToSecondStage { id: library },
        TreeOp::Purge { id: library },
    ] {
        if let Err(e) = tree::apply(state, actor, op).await {
            eprintln!(
                "sharecraft-server: library {} could not be removed after its connection \
                 failed: {e}",
                library.0
            );
            return;
        }
    }
}

/// `GET /api/libraries/{id}/source`: the connection of the library (see [`source_out`]), for
/// anyone who can open the library; `404` otherwise, and for a library that is not connected.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let library = node_id(&id)?;
    // Whoever may see the library as `GET /api/libraries/{id}` shows it (not in the bin).
    libraries::show(&state, &signed.session.user, library).await?;
    let origin = origin(&state, &headers)?;
    let (config, _) = source_of(&state, library).await?;
    let reveal_path = signed.session.user.is_admin
        || config.connected_by == signed.session.user.id
        || state
            .db()
            .effective_access(signed.session.user.id, library)
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .is_some_and(|access| access.can_manage_access());
    Ok(Json(
        answer_of(
            &state,
            &origin,
            library,
            reveal_path,
            signed.session.user.is_admin,
        )
        .await?,
    ))
}

/// The body of `PATCH /api/libraries/{id}/source`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeBody {
    branch: Option<String>,
    folder: Option<String>,
    poll_seconds: Option<u32>,
    /// A new token.
    token: Option<String>,
    /// `true` takes the token away.
    clear_token: Option<bool>,
    /// S3: a new access key id, given with `secret_access_key`.
    access_key_id: Option<String>,
    /// S3: the new key's secret, given with `access_key_id`.
    secret_access_key: Option<SecretText>,
    /// S3: `{list_requests, read_requests, bytes_down}`.
    budget: Option<Value>,
    /// Never changed: named only to be refused with `422`.
    bucket: Option<Value>,
    /// Never changed: named only to be refused with `422`.
    prefix: Option<Value>,
    /// Never changed: named only to be refused with `422`.
    endpoint: Option<Value>,
    /// S3: `read_only` or `read_write`.
    mode: Option<String>,
}

/// `PATCH /api/libraries/{id}/source {branch?, folder?, poll_seconds?, token?,
/// clear_token?}`: changes the connection, for owners of the library (`404` for those who
/// cannot see it, `403` for the others). A new branch, folder or token is probed first, as
/// connecting is (the same `422` answers change nothing), then clears the sync bookmark (for
/// a new branch or folder). Any change puts the connection back to `connecting` and syncs; a
/// sync under way records nothing of its result (see [`crate::source_sync::sync_once`]). A
/// replaced or cleared token's sealed row is deleted. A request that changes nothing writes
/// and records nothing. `422 invalid_request` for both `token` and `clear_token`, or a poll
/// time outside 60 to 86 400 seconds. Answers the connection.
pub(crate) async fn change(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ChangeBody>,
) -> Result<Json<Value>, ApiError> {
    let library = node_id(&id)?;
    let actor = signed.session.user;
    let admin = actor.is_admin;
    require_manager(state.db(), actor.id, library).await?;
    let (config, _) = source_of(&state, library).await?;
    if body.bucket.is_some() || body.prefix.is_some() || body.endpoint.is_some() {
        return Err(ApiError::InvalidRequest);
    }
    if config.kind == SourceKind::S3 {
        let origin = origin(&state, &headers)?;
        return change_s3(state, actor, library, &origin, body, &config).await;
    }
    if body.access_key_id.is_some()
        || body.secret_access_key.is_some()
        || body.budget.is_some()
        || body.mode.is_some()
    {
        return Err(ApiError::InvalidRequest);
    }
    if config.kind == SourceKind::Share
        && (body.branch.is_some()
            || body.folder.is_some()
            || body.token.is_some()
            || body.clear_token.is_some())
    {
        return Err(ApiError::InvalidRequest);
    }
    let clear = body.clear_token == Some(true);
    if clear && body.token.is_some() {
        return Err(ApiError::InvalidRequest);
    }
    if body.branch.as_deref().is_some_and(|b| !valid_branch(b))
        || body.folder.as_deref().is_some_and(|f| !valid_folder(f))
        || body
            .poll_seconds
            .is_some_and(|s| !(records::MIN_POLL_SECONDS..=records::MAX_POLL_SECONDS).contains(&s))
    {
        return Err(ApiError::InvalidRequest);
    }
    let token = body.token.map(checked_token).transpose()?;
    let origin = origin(&state, &headers)?;

    let branch = body.branch.filter(|b| *b != config.branch);
    let folder = body.folder.filter(|f| *f != config.folder);
    let poll_seconds = body.poll_seconds.filter(|s| *s != config.poll_seconds);
    // Clearing a token that is not there changes nothing.
    let clear = clear && config.token.is_some();
    let new_token = token.is_some() || clear;
    if branch.is_none() && folder.is_none() && poll_seconds.is_none() && !new_token {
        return Ok(Json(
            answer_of(&state, &origin, library, true, admin).await?,
        ));
    }
    let reprobe = branch.is_some() || folder.is_some() || new_token;
    if reprobe {
        let repo = RepoRef::parse(&format!("{}/{}", config.owner, config.repo))
            .map_err(|_| ApiError::RepositoryNotFound)?;
        let current = if new_token {
            None
        } else {
            token_of(&state, &config)
                .await
                .map_err(|_| ApiError::TokenRejected)?
        };
        let probing = token.as_ref().or(current.as_ref());
        probe(
            &state,
            &repo,
            Some(branch.as_deref().unwrap_or(&config.branch)),
            folder.as_deref().unwrap_or(&config.folder),
            probing,
        )
        .await?;
    }

    let mut fields: Vec<&str> = Vec::new();
    if branch.is_some() {
        fields.push("branch");
    }
    if folder.is_some() {
        fields.push("folder");
    }
    if poll_seconds.is_some() {
        fields.push("poll_seconds");
    }
    if token.is_some() {
        fields.push("token");
    }
    if clear {
        fields.push("clear_token");
    }
    {
        let state = state.clone();
        to_completion(async move {
            let stored = match token {
                Some(token) => Some(
                    store_secret(&state, token.expose().as_bytes(), &token_context(library))
                        .await?,
                ),
                None => None,
            };
            let change = SourceChange {
                branch,
                folder,
                poll_seconds,
                token: if clear { Some(None) } else { stored.map(Some) },
                webhook_secret: None,
            };
            let displaced = match state.db().change_source(library, &change).await {
                Ok(displaced) => displaced,
                Err(e) => {
                    if let Some(id) = stored {
                        forget_secret(&state, id).await;
                    }
                    return Err(e.into());
                }
            };
            for id in displaced {
                forget_secret(&state, id).await;
            }
            // The change moved the generation on, so a sync under way records nothing of its
            // result: the connection is due again (also one that had stopped, `not_connected`,
            // or waits to retry) and a sync is woken.
            // A new token, branch or folder changes the request and may reset a rate limit.
            // Poll-only changes do not, so an active GitHub retry time must remain authoritative.
            let resets_rate_limit =
                change.token.is_some() || change.branch.is_some() || change.folder.is_some();
            if resets_rate_limit {
                state.db().requeue_source(library, None).await
            } else {
                state
                    .db()
                    .requeue_source_preserving_rate_limit(
                        library,
                        None,
                        time::OffsetDateTime::now_utc(),
                    )
                    .await
            }
            .map_err(|e| ApiError::unavailable(&e))?;
            emit(
                &state,
                "source.changed",
                &actor,
                library,
                json!({ "fields": fields }),
            );
            wake(&state, library);
            Ok(())
        })
        .await?;
    }
    Ok(Json(
        answer_of(&state, &origin, library, true, admin).await?,
    ))
}

/// `POST /api/libraries/{id}/source/writes/{node}/retry`: Retry of a change to the library's
/// store that was accepted and not yet written (waiting, or failed): it becomes due now. For the
/// change's author and the library's owners. `202`; `404` for a node with no pending change or
/// a person who cannot see it; `403` for anyone else.
pub(crate) async fn retry_write(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, node)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let library = node_id(&id)?;
    let node = node_id(&node)?;
    crate::source_write::retry(&state, &signed.session.user, library, node).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// `DELETE /api/libraries/{id}/source/writes/{node}`: Discard of a change to the library's
/// store that was accepted and not yet written, for the change's author and the library's
/// owners. A file the store has shows the store's content again; a new file is deleted. A
/// change whose content is being sent at that moment is withdrawn too; if its bytes land, the
/// next listing takes them as a change made in the store (see
/// [`crate::source_write::discard_write`]). `204`; `404` and `403` as for Retry; `409
/// source_busy` while a listing or a structure change holds the library for more than ten
/// seconds. Recorded as `source.write_discarded`.
pub(crate) async fn discard_write(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, node)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let library = node_id(&id)?;
    let node = node_id(&node)?;
    let actor = signed.session.user;
    to_completion(async move {
        crate::source_write::discard_write(&state, &actor, library, node).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

/// `POST /api/libraries/{id}/source/sync`: "Sync now", for owners of the library. `202`; the
/// sync runs in the background and its outcome shows in the connection's status. It also ends
/// a stop (`not_connected`) and a back-off after failures (`failed`); a rate limit is still
/// waited out, as GitHub would refuse.
pub(crate) async fn sync_now(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let library = node_id(&id)?;
    require_manager(state.db(), signed.session.user.id, library).await?;
    let (_, status) = source_of(&state, library).await?;
    // A sync under way may have read the branch before what is to be synced now: it records
    // nothing of its result, and the one woken here reads the branch afresh.
    if !state
        .db()
        .bump_source_generation(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::NotFound);
    }
    if matches!(
        status.state,
        records::SourceState::NotConnected | records::SourceState::Failed
    ) {
        state
            .db()
            .requeue_source(library, None)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
    }
    wake(&state, library);
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Lower-case hexadecimal of `bytes`.
fn hex(bytes: &[u8]) -> String {
    crate::s3_sign::hex(bytes)
}

/// `POST /api/libraries/{id}/source/webhook`: a new webhook secret for the connection, for
/// owners of the library: 32 random bytes, sealed and stored, answered this once as
/// `{url, secret, replaced}` (`secret` in hexadecimal, as GitHub is given it; `replaced` when
/// it replaces an earlier secret, whose row is deleted).
pub(crate) async fn webhook(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let library = node_id(&id)?;
    let actor = signed.session.user;
    require_manager(state.db(), actor.id, library).await?;
    let (config, _) = source_of(&state, library).await?;
    if config.kind != SourceKind::Github {
        return Err(ApiError::InvalidRequest);
    }
    let origin = origin(&state, &headers)?;
    let mut bytes = [0u8; WEBHOOK_SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| {
        eprintln!("sharecraft-server: no randomness for a webhook secret");
        ApiError::Unavailable
    })?;
    let secret = Secret::new(hex(&bytes));
    let replaced = {
        let state = state.clone();
        let secret = secret.clone();
        to_completion(async move {
            let stored = store_secret(
                &state,
                secret.expose().as_bytes(),
                &webhook_context(library),
            )
            .await?;
            let change = SourceChange {
                webhook_secret: Some(Some(stored)),
                ..SourceChange::default()
            };
            let displaced = match state.db().change_source(library, &change).await {
                Ok(displaced) => displaced,
                Err(e) => {
                    forget_secret(&state, stored).await;
                    return Err(e.into());
                }
            };
            let replaced = !displaced.is_empty();
            for id in displaced {
                forget_secret(&state, id).await;
            }
            // Every change moves the generation on (a sync under way records nothing of its
            // result), so the connection is synced again.
            state
                .db()
                .requeue_source_preserving_rate_limit(
                    library,
                    None,
                    time::OffsetDateTime::now_utc(),
                )
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
            emit(
                &state,
                "source.webhook_set",
                &actor,
                library,
                json!({ "replaced": replaced }),
            );
            wake(&state, library);
            Ok(replaced)
        })
        .await?
    };
    Ok(Json(json!({
        "url": webhook_url(&origin, library),
        "secret": secret.expose(),
        "replaced": replaced,
    })))
}

/// `409 source_writes_pending` while `library` has changes still to be written to its store.
async fn writes_settled(state: &AppState, library: NodeId) -> Result<(), ApiError> {
    let (saving, failed) = state
        .db()
        .pending_counts(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    if saving > 0 || failed > 0 {
        return Err(ApiError::SourceWritesPending);
    }
    Ok(())
}

/// `DELETE /api/libraries/{id}/source`: disconnects the library, for owners of it: its files
/// stay as they are and it becomes an ordinary library again; the connection's sealed secrets
/// are deleted. `204`; `404` for a library that is not connected; `409 source_busy` while a
/// sync of it runs; `409 source_writes_pending` while changes to its store are still to be
/// written, before anything is removed. The library's sync turn is held until everything is removed, so no sync
/// changes the library after the answer. Recorded as `source.disconnected`.
pub(crate) async fn disconnect(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let library = node_id(&id)?;
    let actor = signed.session.user;
    require_manager(state.db(), actor.id, library).await?;
    let (config, _) = source_of(&state, library).await?;
    // Changes accepted for the store and not yet written are not dropped by a disconnect.
    writes_settled(&state, library).await?;
    // A file share and a bucket are listed in long passes that a disconnect stops: their
    // generation moves on and the disconnect waits for the pass to give up its turn.
    let external = config.kind != SourceKind::Github;
    to_completion(async move {
        // While this runs no sync of the library starts, a listing under way gives up at its
        // next folder, and one that ends is not made due again.
        let stopping = external.then(|| state.sources().stop(library));
        if external {
            state
                .db()
                .bump_source_generation(library)
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let turn = loop {
            if let Some(turn) = state
                .db()
                .try_lock_source(library)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
            {
                break turn;
            }
            if !external || tokio::time::Instant::now() >= deadline {
                return Err(ApiError::SourceBusy);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        // Judged again once the turn is held, before anything is removed, and closed to new
        // changes in the same step: the library is made read-only in one transaction that
        // locks its connection row and counts its pending changes (a stage holds that row
        // `FOR SHARE`, so it is either counted here, and the disconnect is refused with nothing
        // moved, or it waits and then finds the library read-only and is refused). No change
        // can be accepted once the children are being moved. A disconnect that fails after
        // this leaves the library read-only, which an owner may choose anyway.
        let ended = async {
            let closed = state
                .db()
                .set_source_mode(library, SourceMode::ReadOnly)
                .await?;
            if closed {
                recheck_below(&state, library).await;
            }
            let removed = if external {
                tree::delete_source_children(&state, &actor, library).await?
            } else {
                0
            };
            // The library-level authorisation is judged again in the disconnect's own
            // transaction, also when there were no children to remove: the route's check above
            // was made before the wait for the source turn.
            let secrets = state.db().disconnect_source_as(actor.id, library).await?;
            Ok::<_, ApiError>((removed, secrets))
        }
        .await;
        let (removed, secrets) = match ended {
            Ok(ended) => ended,
            Err(error) => {
                // Still connected: a listing stopped for this disconnect was not made due
                // again, so it is made due here.
                if external && let Err(e) = state.db().requeue_source(library, None).await {
                    eprintln!("sharecraft-server: database failure: {e}");
                }
                return Err(error);
            }
        };
        for id in secrets {
            forget_secret(&state, id).await;
        }
        drop(turn);
        drop(stopping);
        let detail = if external {
            json!({ "kind": config.kind.as_str(), "removed": removed })
        } else {
            json!({ "repository": format!("{}/{}", config.owner, config.repo) })
        };
        emit(&state, "source.disconnected", &actor, library, detail);
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

/// A secret key as a request carries it, wrapped in [`Secret`] as it is read: the body holds
/// no plain string a `Debug` or an error could print. No `Debug` of its own.
pub(crate) struct SecretText(Secret);

impl<'de> Deserialize<'de> for SecretText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| SecretText(Secret::new(text)))
    }
}

/// `id` if it can be an access key id: 1 to 128 bytes, no control character.
fn checked_key_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty() || id.len() > MAX_KEY_ID_BYTES || id.chars().any(char::is_control) {
        return Err(ApiError::InvalidRequest);
    }
    Ok(())
}

/// `secret` if it can be a secret access key: 1 to 1,000 bytes, no control character.
fn checked_secret_key(secret: &Secret) -> Result<(), ApiError> {
    let text = secret.expose();
    if text.is_empty() || text.len() > MAX_SECRET_KEY_BYTES || text.chars().any(char::is_control) {
        return Err(ApiError::InvalidRequest);
    }
    Ok(())
}

/// The approved endpoint named `name`, read afresh; `422 s3_endpoint_not_approved` for a name
/// no administrator approved (an address typed in its place is no name).
async fn approved_endpoint(state: &AppState, name: &str) -> Result<S3Endpoint, ApiError> {
    state
        .s3()
        .approved_endpoint(state, name)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::S3EndpointNotApproved)
}

/// The answer for an S3 failure while probing: a reason word, never the store's text.
fn s3_refused(error: S3Error) -> ApiError {
    let reason = match error {
        S3Error::BadKeys => "keys",
        S3Error::NoBucket | S3Error::NotFound => "bucket",
        S3Error::Denied => "denied",
        S3Error::WrongRegion => "region",
        S3Error::ClockSkew => "clock",
        S3Error::AddressRefused => "address",
        S3Error::Throttled
        | S3Error::Precondition
        | S3Error::Archived
        | S3Error::Unsupported
        | S3Error::Unreachable
        | S3Error::TimedOut
        | S3Error::Malformed
        | S3Error::Other => {
            // `S3Error` holds no text from the store, no key and no address.
            eprintln!("sharecraft-server: an S3 bucket could not be probed: {error}");
            "offline"
        }
    };
    ApiError::S3Unreachable(reason)
}

/// Asks the bucket whether the key may list `prefix` (one listing of at most one key), then
/// its versioning state (whose failure is no refusal: it is recorded as unknown). Two
/// requests, no write, answered before `deadline` (20 seconds from the start of the probe, the
/// write probe included when there is one). The connect probe (`library` is `None`: the
/// library does not exist yet) is counted against no library; a key replacement is counted
/// against its library.
#[allow(clippy::too_many_arguments)]
async fn probe_bucket(
    state: &AppState,
    endpoint: &S3Endpoint,
    bucket: &str,
    prefix: &str,
    access_key_id: &str,
    secret: &Secret,
    library: Option<NodeId>,
    deadline: tokio::time::Instant,
) -> Result<Versioning, ApiError> {
    let s3 = state.s3();
    let mut target = s3.target_from(
        endpoint,
        bucket,
        Credentials {
            access_key_id: access_key_id.to_owned(),
            secret: secret.clone(),
            session_token: None,
        },
    );
    if let Some(library) = library {
        target = target.with_library(library);
    }
    let probing = async {
        s3.list_page(&target, prefix, None, 1)
            .await
            .map_err(s3_refused)?;
        Ok::<_, ApiError>(
            s3.bucket_versioning(&target)
                .await
                .unwrap_or(Versioning::Unknown),
        )
    };
    tokio::time::timeout_at(deadline, probing)
        .await
        .map_err(|_| ApiError::S3Unreachable("offline"))?
}

/// The write probe (see [`s3_write::probe_writes`]) on `target`, answered before `deadline`.
/// It runs as a task of its own, so the limit never stops it between its first write and its
/// removal: one answered too late is `offline` here and still cleans up after itself (an
/// object it then could not remove is logged).
async fn probe_writes_by(
    state: &AppState,
    target: S3Target,
    deadline: tokio::time::Instant,
    library: Option<NodeId>,
) -> (Probe, Option<String>) {
    let mut task = {
        let state = state.clone();
        tokio::spawn(async move { s3_write::probe_writes(state.s3(), &target).await })
    };
    match tokio::time::timeout_at(deadline, &mut task).await {
        Ok(Ok(found)) => found,
        Ok(Err(_)) => (Probe::Unreachable("offline"), None),
        Err(_) => {
            tokio::spawn(async move {
                if let Ok((probe, Some(_))) = task.await
                    && !matches!(probe, Probe::Unreachable(_))
                {
                    log_probe_left(library);
                }
            });
            (Probe::Unreachable("offline"), None)
        }
    }
}

fn log_probe_left(library: Option<NodeId>) {
    match library {
        Some(library) => eprintln!(
            "sharecraft-server: a write probe object could not be removed (library {})",
            library.0
        ),
        None => eprintln!(
            "sharecraft-server: a write probe object could not be removed (no library was made)"
        ),
    }
}

/// Logs and announces (`source.probe_left {name}`, the name relative to the library's place:
/// never the bucket or the prefix) a probe object that could not be removed. Nothing when the
/// probe could not write at all: then no object was made.
fn report_probe_left(
    state: &AppState,
    actor: &User,
    library: Option<NodeId>,
    probe: Probe,
    left: Option<String>,
) {
    let Some(name) = left else {
        return;
    };
    if matches!(probe, Probe::Unreachable(_)) {
        return;
    }
    log_probe_left(library);
    state.events().emit(NewEvent {
        kind: "source.probe_left",
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: library,
        detail: json!({ "name": name }),
    });
}

/// What the write probe allows: read and write with the condition support found, or the
/// refusal (`422 s3_not_writable` for a key that may only read, `422 s3_unreachable`).
fn writes_allowed(probe: Probe) -> Result<Tri, ApiError> {
    match probe {
        Probe::ReadWrite { conditional } => Ok(conditional),
        Probe::ReadOnly => Err(ApiError::S3NotWritable),
        Probe::Unreachable(reason) => Err(ApiError::S3Unreachable(reason)),
    }
}

/// The last four characters of an access key id, as the pages show it.
fn id_ends(id: &str) -> String {
    let start = id.char_indices().rev().nth(3).map_or(0, |(index, _)| index);
    id.get(start..).unwrap_or_default().to_owned()
}

/// An S3 connection as the routes answer it. Its place, key, usage, budget and pause go only
/// to those who may manage the library (`reveal`); everyone else gets nulls for them. Never
/// the secret and never the endpoint's address: the endpoint by its name only.
async fn s3_answer(
    state: &AppState,
    config: &SourceConfig,
    status: &SourceStatus,
    reveal: bool,
    admin: bool,
) -> Result<Value, ApiError> {
    let library = config.library;
    let db = state.db();
    let (write_state, write_error) = db
        .write_state(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .unwrap_or((WriteState::Ok, None));
    let (saving, failed) = db
        .pending_counts(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let listing_now = (status.state == records::SourceState::Syncing)
        .then(|| state.s3().listing_progress(library))
        .flatten()
        .map(|files| json!({ "files_so_far": files }));
    let mut answer = json!({
        "kind": "s3",
        "mode": config.mode.as_str(),
        "state": status.state.as_str(),
        "last_checked_at": rfc3339(status.last_checked_at),
        "last_synced_at": rfc3339(status.last_synced_at),
        "last_error": status.last_error,
        "files": status.files,
        "bytes": status.bytes,
        "hidden": status.hidden,
        "hidden_by": status.hidden_detail,
        "poll_seconds": config.poll_seconds,
        "next_check_at": rfc3339(status.next_check_at),
        "listing_now": listing_now,
        "write_state": write_state.as_str(),
        "write_error": write_error,
        "writes": { "saving": saving, "failed": failed },
        "may_choose_read_write": admin,
        "conditional": null,
        "endpoint": null,
        "bucket": null,
        "prefix": null,
        "key": null,
        "versioning": null,
        "usage": null,
        "budget": null,
        "paused": null,
    });
    if !reveal {
        return Ok(answer);
    }
    let Some(place) = db
        .s3_place(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    else {
        return Ok(answer);
    };
    let today = time::OffsetDateTime::now_utc().date();
    let stored = db
        .usage(library, today)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    // What is counted and not yet stored (the worker stores it) counts too.
    let unflushed = state.s3().unflushed_usage(library);
    let usage = Usage {
        list: stored.list.saturating_add(unflushed.list),
        read: stored.read.saturating_add(unflushed.read),
        write: stored.write.saturating_add(unflushed.write),
        bytes_down: stored.bytes_down.saturating_add(unflushed.bytes_down),
        bytes_up: stored.bytes_up.saturating_add(unflushed.bytes_up),
        cache_hits: stored.cache_hits.saturating_add(unflushed.cache_hits),
    };
    let budget = db
        .budget(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let paused = if usage.list >= budget.list_requests {
        Some("list")
    } else if usage.read >= budget.read_requests
        || budget
            .bytes_down
            .is_some_and(|most| usage.bytes_down >= most)
    {
        Some("read")
    } else {
        None
    };
    if let Some(object) = answer.as_object_mut() {
        for (field, value) in [
            ("endpoint", json!(place.endpoint)),
            ("bucket", json!(place.bucket)),
            ("prefix", json!(place.prefix)),
            (
                "key",
                json!({ "id_ends": id_ends(&place.access_key_id), "secret": "set" }),
            ),
            ("versioning", json!(place.versioning.as_str())),
            ("conditional", json!(place.conditional.as_str())),
            (
                "usage",
                json!({
                    "month": format!("{:04}-{:02}", today.year(), u8::from(today.month())),
                    "list": usage.list,
                    "read": usage.read,
                    "write": usage.write,
                    "bytes_down": usage.bytes_down,
                    "bytes_up": usage.bytes_up,
                    "cache_hits": usage.cache_hits,
                }),
            ),
            (
                "budget",
                json!({
                    "list_requests": budget.list_requests,
                    "read_requests": budget.read_requests,
                    "bytes_down": budget.bytes_down,
                }),
            ),
            ("paused", json!(paused)),
        ] {
            object.insert(field.to_owned(), value);
        }
    }
    Ok(answer)
}

/// The body of `POST /api/sites/{id}/libraries/s3`. No `Debug`: it holds the secret key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConnectS3Body {
    name: String,
    /// The name of an approved endpoint, or `aws:<region>`.
    endpoint: String,
    bucket: String,
    /// The whole bucket when not given.
    prefix: Option<String>,
    access_key_id: String,
    secret_access_key: SecretText,
    /// `document` when not given, or `image`.
    kind: Option<String>,
    /// `read_only` when not given, or `read_write` (administrators only).
    mode: Option<String>,
}

/// `POST /api/sites/{id}/libraries/s3 {name, endpoint, bucket, prefix?, access_key_id,
/// secret_access_key, kind?, mode?}`: makes a new library in the site backed by the bucket
/// (below the prefix), for someone who may make a library there; read-only unless `mode` is
/// `read_write`. Checked in this order, nothing asked of the store before the last: access
/// (`403`); `kind` `document` or `image` and `mode` `read_only` or `read_write` (`422
/// invalid_request`); `read_write` from someone who is not an administrator (`403
/// read_write_admin_only`); the endpoint is approved (`422 s3_endpoint_not_approved`); the
/// bucket and prefix (`422 invalid_s3_place` with `reason` `bucket` or `prefix`); the key's
/// lengths (`422 invalid_request`); the probe (`422 s3_unreachable` with its `reason`), and for
/// `read_write` the write probe in the same 20 seconds (`422 s3_not_writable` for a key that
/// may only read; it leaves nothing in the bucket). Only then is the library made, the secret
/// sealed and the
/// connection recorded (`409 s3_place_in_use` when another library holds an overlapping prefix
/// of the bucket, judged together with the endpoint's approval under one lock); a failure
/// removes the new library and forgets the secret. `201 {id, source}`.
pub(crate) async fn connect_s3(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ConnectS3Body>,
) -> Result<Response, ApiError> {
    let ConnectS3Body {
        name,
        endpoint,
        bucket,
        prefix,
        access_key_id,
        secret_access_key: SecretText(secret),
        kind,
        mode,
    } = body;
    let site = node_id(&id)?;
    let actor = signed.session.user;

    // Who may make a library here, judged before anything else.
    let (place, access) = tree::visible(state.db(), &actor, site).await?;
    if place.kind != records::NodeKind::Site {
        return Err(ApiError::WrongPlace);
    }
    tree::require(&access, Perms::MANAGE_SITE)?;
    let kind = match kind.as_deref() {
        None | Some("document") => LibraryKind::Document,
        Some("image") => LibraryKind::Image,
        Some(_) => return Err(ApiError::InvalidRequest),
    };
    let mode = match mode.as_deref() {
        None | Some("read_only") => SourceMode::ReadOnly,
        Some("read_write") => SourceMode::ReadWrite,
        Some(_) => return Err(ApiError::InvalidRequest),
    };
    if mode == SourceMode::ReadWrite && !actor.is_admin {
        return Err(ApiError::ReadWriteAdminOnly);
    }
    let name = records::validate_node_name(&name)
        .map_err(ApiError::from)?
        .to_owned();
    let endpoint = approved_endpoint(&state, &endpoint).await?;
    if !valid_bucket(&bucket) {
        return Err(ApiError::InvalidS3Place("bucket"));
    }
    let prefix = clean_prefix(prefix.as_deref().unwrap_or_default())
        .ok_or(ApiError::InvalidS3Place("prefix"))?;
    checked_key_id(&access_key_id)?;
    checked_secret_key(&secret)?;
    // A taken name costs no request to the store; `create_library` checks it again.
    if !state
        .db()
        .name_free(site, &name, None)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::NameTaken);
    }

    // A place another library holds is refused before the store is asked: the write probe
    // must never write into another library's place (`connect_s3_source` judges it again).
    if state
        .db()
        .s3_place_taken(&endpoint.name, &bucket, &prefix)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::S3PlaceInUse);
    }
    let deadline = tokio::time::Instant::now() + S3_PROBE_LIMIT;
    let versioning = probe_bucket(
        &state,
        &endpoint,
        &bucket,
        &prefix,
        &access_key_id,
        &secret,
        None,
        deadline,
    )
    .await?;
    let (conditional, probed) = if mode == SourceMode::ReadWrite {
        let target = state
            .s3()
            .target_from(
                &endpoint,
                &bucket,
                Credentials {
                    access_key_id: access_key_id.clone(),
                    secret: secret.clone(),
                    session_token: None,
                },
            )
            .with_prefix(&prefix);
        let (probe, left) = probe_writes_by(&state, target, deadline, None).await;
        match writes_allowed(probe) {
            Ok(conditional) => (conditional, Some((probe, left))),
            Err(e) => {
                report_probe_left(&state, &actor, None, probe, left);
                return Err(e);
            }
        }
    } else {
        (Tri::Unknown, None)
    };
    let library = NodeId(Uuid::new_v4());
    let origin = origin(&state, &headers)?;
    {
        let made = {
            let state = state.clone();
            let actor = actor.clone();
            to_completion(async move {
                tree::create_library(&state, &actor, site, library, &name, kind).await?;
                let sealed =
                    store_secret(&state, secret.expose().as_bytes(), &token_context(library)).await;
                drop(secret);
                let token = match sealed {
                    Ok(token) => token,
                    Err(e) => {
                        purge_new_library(&state, &actor, library).await;
                        return Err(e);
                    }
                };
                let place = S3Place {
                    endpoint: endpoint.name,
                    bucket,
                    prefix,
                    access_key_id,
                    versioning,
                    conditional,
                };
                let connected = state
                    .db()
                    .connect_s3_source(
                        &NewSource {
                            library,
                            kind: SourceKind::S3,
                            owner: String::new(),
                            repo: String::new(),
                            branch: String::new(),
                            folder: String::new(),
                            location: Some(s3_location(
                                &place.endpoint,
                                &place.bucket,
                                &place.prefix,
                            )),
                            mode,
                            token: Some(token),
                            poll_seconds: DEFAULT_POLL_SECONDS,
                            connected_by: actor.id,
                        },
                        &place,
                    )
                    .await;
                if let Err(e) = connected {
                    purge_new_library(&state, &actor, library).await;
                    forget_secret(&state, token).await;
                    return Err(match e {
                        AccessError::Exists => ApiError::S3PlaceInUse,
                        e => e.into(),
                    });
                }
                abandon_uploads_into(&state, library).await;
                emit(
                    &state,
                    "source.connected",
                    &actor,
                    library,
                    json!({ "kind": "s3" }),
                );
                wake(&state, library);
                Ok(())
            })
            .await
        };
        if let Some((probe, left)) = probed {
            report_probe_left(&state, &actor, made.is_ok().then_some(library), probe, left);
        }
        made?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": library.0.to_string(),
            "source": answer_of(&state, &origin, library, true, actor.is_admin).await?,
        })),
    )
        .into_response())
}

/// A budget as `PATCH` takes it: `{list_requests, read_requests, bytes_down}`, the first two
/// whole numbers of at least 1, `bytes_down` one too or `null` (the server's default). Anything
/// else is `422 invalid_request`.
fn budget_of(value: &Value) -> Result<Budget, ApiError> {
    let object = value.as_object().ok_or(ApiError::InvalidRequest)?;
    if object.len() != 3 {
        return Err(ApiError::InvalidRequest);
    }
    let count = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_u64)
            .filter(|n| *n >= 1 && i64::try_from(*n).is_ok())
            .ok_or(ApiError::InvalidRequest)
    };
    let bytes_down = match object.get("bytes_down") {
        Some(Value::Null) => None,
        Some(_) => Some(count("bytes_down")?),
        None => return Err(ApiError::InvalidRequest),
    };
    Ok(Budget {
        list_requests: count("list_requests")?,
        read_requests: count("read_requests")?,
        bytes_down,
    })
}

/// `PATCH /api/libraries/{id}/source` for an S3 library: `{poll_seconds?, access_key_id? with
/// secret_access_key?, budget?, mode?}`. A GitHub field is `422 invalid_request`, and so is a
/// key id without its secret or the other way round, a `mode` other than `read_only` or
/// `read_write`, and a `mode` given with a new key. A new key is probed first, as connecting is
/// (the same `422` answers change nothing); then it replaces the old one, whose sealed row is
/// deleted, and a library that could not connect is tried again.
///
/// `mode` `read_write` is for administrators only (`403 read_write_admin_only`, before the
/// store is asked): the write probe runs first with the library's key, counted against it
/// (`422 s3_not_writable`, `422 s3_unreachable`, nothing changed); a pass records what it found
/// about conditional writes, makes the library read and write and its writes no longer
/// blocked (which is how a blocked library is restored). `read_only` is for owners; `409
/// source_writes_pending` while changes are still to be written. A mode change is recorded as
/// `source.mode_changed {mode}` and the open files below are rechecked. Answers the connection.
async fn change_s3(
    state: AppState,
    actor: User,
    library: NodeId,
    origin: &str,
    body: ChangeBody,
    config: &SourceConfig,
) -> Result<Json<Value>, ApiError> {
    let ChangeBody {
        branch,
        folder,
        poll_seconds,
        token,
        clear_token,
        access_key_id,
        secret_access_key,
        budget,
        mode,
        ..
    } = body;
    let admin = actor.is_admin;
    let mode = match mode.as_deref() {
        None => None,
        Some("read_only") => Some(SourceMode::ReadOnly),
        Some("read_write") => Some(SourceMode::ReadWrite),
        Some(_) => return Err(ApiError::InvalidRequest),
    };
    if mode == Some(SourceMode::ReadWrite) && !admin {
        return Err(ApiError::ReadWriteAdminOnly);
    }
    if branch.is_some() || folder.is_some() || token.is_some() || clear_token.is_some() {
        return Err(ApiError::InvalidRequest);
    }
    if poll_seconds
        .is_some_and(|s| !(records::MIN_POLL_SECONDS..=records::MAX_POLL_SECONDS).contains(&s))
    {
        return Err(ApiError::InvalidRequest);
    }
    let budget = budget.as_ref().map(budget_of).transpose()?;
    let key = match (access_key_id, secret_access_key) {
        (None, None) => None,
        (Some(id), Some(SecretText(secret))) => {
            checked_key_id(&id)?;
            checked_secret_key(&secret)?;
            Some((id, secret))
        }
        _ => return Err(ApiError::InvalidRequest),
    };
    if mode.is_some() && key.is_some() {
        return Err(ApiError::InvalidRequest);
    }
    // A mode asked for that the library has changes nothing; read and write asked for again
    // while its writes are blocked probes and restores it.
    let mode = match mode {
        Some(SourceMode::ReadWrite) if config.mode == SourceMode::ReadWrite => {
            let blocked = matches!(
                state
                    .db()
                    .write_state(library)
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?,
                Some((WriteState::Blocked, _))
            );
            blocked.then_some(SourceMode::ReadWrite)
        }
        Some(mode) => (mode != config.mode).then_some(mode),
        None => None,
    };
    let poll_seconds = poll_seconds.filter(|s| *s != config.poll_seconds);
    let budget = match budget {
        Some(budget) => {
            let current = state
                .db()
                .budget(library)
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
            (budget != current).then_some(budget)
        }
        None => None,
    };
    if poll_seconds.is_none() && budget.is_none() && key.is_none() && mode.is_none() {
        return Ok(Json(answer_of(&state, origin, library, true, admin).await?));
    }
    // Read and write only once the store has shown it takes writes, with the library's own key
    // (counted against it), before anything changes.
    let writable = if mode == Some(SourceMode::ReadWrite) {
        let target = state
            .s3()
            .target(&state, library)
            .await
            .map_err(s3_refused)?;
        let deadline = tokio::time::Instant::now() + S3_PROBE_LIMIT;
        let (probe, left) = probe_writes_by(&state, target, deadline, Some(library)).await;
        report_probe_left(&state, &actor, Some(library), probe, left);
        Some(writes_allowed(probe)?)
    } else {
        None
    };
    let probed = match &key {
        Some((id, secret)) => {
            let place = state
                .db()
                .s3_place(library)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::NotFound)?;
            let endpoint = approved_endpoint(&state, &place.endpoint).await?;
            let versioning = probe_bucket(
                &state,
                &endpoint,
                &place.bucket,
                &place.prefix,
                id,
                secret,
                Some(library),
                tokio::time::Instant::now() + S3_PROBE_LIMIT,
            )
            .await?;
            Some((versioning, place.conditional))
        }
        None => None,
    };
    let mut fields: Vec<&str> = Vec::new();
    if poll_seconds.is_some() {
        fields.push("poll_seconds");
    }
    if key.is_some() {
        fields.push("key");
    }
    if budget.is_some() {
        fields.push("budget");
    }
    {
        let state = state.clone();
        to_completion(async move {
            // First, so that a refusal (changes still to be written) changes nothing else.
            if let Some(mode) = mode {
                change_mode(&state, &actor, library, mode, writable).await?;
            }
            if let Some((id, secret)) = key {
                let stored =
                    store_secret(&state, secret.expose().as_bytes(), &token_context(library)).await;
                drop(secret);
                let stored = stored?;
                match state.db().set_s3_key(library, &id, stored).await {
                    Ok(displaced) => {
                        if let Some(old) = displaced {
                            forget_secret(&state, old).await;
                        }
                    }
                    Err(e) => {
                        forget_secret(&state, stored).await;
                        return Err(e.into());
                    }
                }
                if let Some((versioning, conditional)) = probed {
                    state
                        .db()
                        .set_s3_probe(library, versioning, conditional)
                        .await
                        .map_err(|e| ApiError::unavailable(&e))?;
                }
            }
            if let Some(poll_seconds) = poll_seconds {
                let change = SourceChange {
                    poll_seconds: Some(poll_seconds),
                    ..SourceChange::default()
                };
                state.db().change_source(library, &change).await?;
                state
                    .db()
                    .requeue_source_preserving_rate_limit(
                        library,
                        None,
                        time::OffsetDateTime::now_utc(),
                    )
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?;
            }
            if let Some(budget) = budget {
                state.db().set_budget(library, &budget).await?;
            }
            if !fields.is_empty() {
                emit(
                    &state,
                    "source.changed",
                    &actor,
                    library,
                    json!({ "fields": fields }),
                );
                wake(&state, library);
            }
            Ok(())
        })
        .await?;
    }
    Ok(Json(answer_of(&state, origin, library, true, admin).await?))
}

/// Makes `library` `mode`. For read and write, `conditional` is what the write probe found,
/// recorded first; the writes are then no longer blocked. A library with changes still to be
/// written is not made read-only (`409 source_writes_pending`, judged in the same transaction
/// as the change). A real change is recorded and the open files below are rechecked.
async fn change_mode(
    state: &AppState,
    actor: &User,
    library: NodeId,
    mode: SourceMode,
    conditional: Option<Tri>,
) -> Result<(), ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    if let Some(conditional) = conditional {
        let versioning = db
            .s3_place(library)
            .await
            .map_err(unavailable)?
            .ok_or(ApiError::NotFound)?
            .versioning;
        db.set_s3_probe(library, versioning, conditional)
            .await
            .map_err(unavailable)?;
    }
    let changed = db.set_source_mode(library, mode).await?;
    if mode == SourceMode::ReadWrite {
        let unblocked = db
            .set_write_state(library, WriteState::Ok, None)
            .await
            .map_err(unavailable)?;
        // A blocked library restored, as the worker's passing probe does it: the changes
        // waiting are due at once, and the seats narrowed by the block widen again.
        if unblocked {
            source_write::restored(state, library, true, time::OffsetDateTime::now_utc()).await;
        }
    }
    if changed {
        emit(
            state,
            "source.mode_changed",
            actor,
            library,
            json!({ "mode": mode.as_str() }),
        );
        recheck_below(state, library).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{hex, valid_branch, valid_folder};

    #[test]
    fn branch_names() {
        for good in ["main", "release/1.0", "feature-x", "v1.2.3", "a.b"] {
            assert!(valid_branch(good), "{good}");
        }
        for bad in [
            "", ".", "..", "a..b", "-x", "/x", "x/", "x.lock", "x@{1}", "a\\b", "a\nb",
        ] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
        assert!(!valid_branch(&"a".repeat(201)));
    }

    #[test]
    fn folder_paths() {
        for good in ["", "docs", "docs/guide", "a.b/c"] {
            assert!(valid_folder(good), "{good}");
        }
        for bad in [
            "/docs", "docs/", "a//b", "../x", "a/../b", ".", "a\\b", "c:/x", "a\tb",
        ] {
            assert!(!valid_folder(bad), "{bad:?}");
        }
        assert!(!valid_folder(&"a".repeat(501)));
    }

    #[test]
    fn the_last_four_characters_of_a_key_id() {
        assert_eq!(super::id_ends("AKIAEXAMPLE1234"), "1234");
        assert_eq!(super::id_ends("ab"), "ab");
    }

    #[test]
    fn hex_is_lower_case() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }
}
