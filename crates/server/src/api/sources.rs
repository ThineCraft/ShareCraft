//! Connecting a library to a branch of a GitHub repository and managing the connection:
//! connect (a new library, made only once GitHub answers that the repository and branch are
//! there and within the caps), show, change, "Sync now", the webhook secret and disconnect.
//!
//! Secrets (the token, the webhook secret) are sealed (see [`crate::secrets`]) before they
//! reach the database, and no answer, event, log line or error carries one: the only exception
//! is the webhook secret in the one answer that generates it.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::{
    LibraryKind, NewEvent, NewSource, Perms, SealedRow, SecretId, SourceChange, SourceConfig,
    SourceKind, SourceStatus, User,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::nodes::{node_id, require_manager, rfc3339};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::github::{EntryKind, GithubError, Head, RepoRef};
use crate::libraries;
use crate::secrets::Secret;
use crate::source_sync::{abandon_uploads_into, caps, token_context, token_of, webhook_context};
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
fn source_out(origin: &str, config: &SourceConfig, status: &SourceStatus) -> Value {
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
async fn answer_of(state: &AppState, origin: &str, library: NodeId) -> Result<Value, ApiError> {
    let (config, status) = source_of(state, library).await?;
    Ok(source_out(origin, &config, &status))
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
    let source = answer_of(&state, &origin, library).await?;
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

/// Removes `library`, made a moment ago by `actor` for a connection that then failed, for good:
/// to the recycle bin, its second stage, and purged, so it is in neither the site nor the bin.
/// A failure is logged.
async fn purge_new_library(state: &AppState, actor: &User, library: NodeId) {
    for op in [
        TreeOp::Delete { id: library },
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
    Ok(Json(answer_of(&state, &origin, library).await?))
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
    require_manager(state.db(), actor.id, library).await?;
    let (config, _) = source_of(&state, library).await?;
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
        return Ok(Json(answer_of(&state, &origin, library).await?));
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
            state
                .db()
                .requeue_source(library, None)
                .await
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
    Ok(Json(answer_of(&state, &origin, library).await?))
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
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        for nibble in [b >> 4, b & 0x0f] {
            if let Some(&digit) = DIGITS.get(usize::from(nibble)) {
                out.push(char::from(digit));
            }
        }
    }
    out
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
    source_of(&state, library).await?;
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
                .requeue_source(library, None)
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

/// `DELETE /api/libraries/{id}/source`: disconnects the library, for owners of it: its files
/// stay as they are and it becomes an ordinary library again; the connection's sealed secrets
/// are deleted. `204`; `404` for a library that is not connected; `409 source_busy` while a
/// sync of it runs. The library's sync turn is held until everything is removed, so no sync
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
    to_completion(async move {
        let turn = state
            .db()
            .try_lock_source(library)
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .ok_or(ApiError::SourceBusy)?;
        let secrets = state
            .db()
            .disconnect_source(library)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        for id in secrets {
            forget_secret(&state, id).await;
        }
        drop(turn);
        emit(
            &state,
            "source.disconnected",
            &actor,
            library,
            json!({ "repository": format!("{}/{}", config.owner, config.repo) }),
        );
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
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
    fn hex_is_lower_case() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }
}
