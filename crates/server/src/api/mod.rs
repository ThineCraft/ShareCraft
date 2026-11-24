//! The JSON API under `/api`. Every request must pass the origin check
//! ([`crate::deployment::Deployment::same_origin`]), and any request other than `GET` must also
//! carry `Origin`. Errors are `{"error": "<code>"}`.

mod admin;
mod admin_looks;
mod bin;
pub(crate) mod brand;
mod bulk;
pub(crate) mod content;
mod docs;
mod explorer;
mod groups;
mod libraries;
mod links;
mod looks;
mod navigation;
pub(crate) mod nodes;
mod quarantine;
mod requests;
mod roles;
mod session;
mod settings;
mod sites;
mod sources;
mod templates;
pub(crate) mod uploads;

use std::fmt;

use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::http::header::{CACHE_CONTROL, ORIGIN};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use collab_core::ids::{DocId, NodeId, SessionId, UserId};
use records::{
    AccessError, AccountError, FileType, LinkError, RequestError, ScanState, SourceKind,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::deployment::SameOrigin;
use crate::room::AccessChange;

/// Largest request body accepted.
const MAX_BODY: usize = 16 * 1024;

/// The `message` of `409 source_read_only`.
const SOURCE_READ_ONLY: &str =
    "This library is kept in step with an outside source, so it cannot be changed here.";

/// The `message` of `409 source_writes_pending`.
const SOURCE_WRITES_PENDING: &str =
    "Some changes are still being saved to the storage. Wait for them, or discard them, first.";

/// The `message` of `409 source_action_unavailable`.
const SOURCE_ACTION_UNAVAILABLE: &str = "This cannot be done yet in a library stored in S3. Uploading files, saving and making folders work; renaming, moving, copying and deleting come later.";

/// The `message` of `409 source_write_blocked`.
const SOURCE_WRITE_BLOCKED: &str = "This library is read-only until its storage accepts changes again. Its files can still be opened.";

/// The `message` of `422 s3_not_writable`.
const S3_NOT_WRITABLE: &str = "This access key may read the bucket but not write to it. Connect the library read-only, or use a key that may write.";

/// The `message` of `403 read_write_admin_only`.
const READ_WRITE_ADMIN_ONLY: &str =
    "Only an administrator may let people change the files in a bucket.";

const SOURCE_COPY_UNAVAILABLE: &str = "Files in this library are read from outside storage when \
opened and cannot be copied here yet. Download the file instead.";

/// The `/api` routes, to be nested under `/api`. Unknown paths answer `404 {"error":"not_found"}`
/// and pass the same guard as the others. File content is the only thing allowed a body larger
/// than [`MAX_BODY`]: it is streamed into the content store, up to the largest file allowed
/// (`max_file_bytes`), which the content routes check themselves, or into an upload's part file,
/// up to the size the upload declared.
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/session", post(session::sign_in).delete(session::sign_out))
        .route("/me", get(session::me))
        .route("/editors", get(crate::editors::list))
        .route("/sites", get(sites::list).post(sites::create))
        .route(
            "/sites/{id}",
            get(sites::show).patch(sites::change).delete(sites::remove),
        )
        .route("/sites/{id}/quota", put(sites::set_quota))
        .route("/sites/{id}/archive", post(sites::archive))
        .route("/sites/{id}/reactivate", post(sites::reactivate))
        .route("/sites/{id}/groups", get(sites::groups))
        .route("/sites/{id}/libraries", post(libraries::create))
        .route("/sites/{id}/libraries/github", post(sources::connect))
        .route("/sites/{id}/libraries/share", post(sources::connect_share))
        .route("/sites/{id}/libraries/s3", post(sources::connect_s3))
        .route(
            "/libraries/{id}/source",
            get(sources::show)
                .patch(sources::change)
                .delete(sources::disconnect),
        )
        .route("/libraries/{id}/source/sync", post(sources::sync_now))
        .route(
            "/libraries/{id}/source/writes/{node}/retry",
            post(sources::retry_write),
        )
        .route(
            "/libraries/{id}/source/writes/{node}",
            delete(sources::discard_write),
        )
        .route("/libraries/{id}/source/webhook", post(sources::webhook))
        .route(
            "/settings/shares",
            get(settings::shares).put(settings::change_shares),
        )
        .route("/settings/s3", get(settings::s3).put(settings::change_s3))
        .route("/s3/endpoints", get(settings::s3_names))
        .route(
            "/libraries/{id}",
            get(libraries::show).patch(libraries::change),
        )
        .route("/libraries/{id}/templates", get(libraries::templates))
        .route(
            "/libraries/{id}/from-template",
            post(libraries::from_template),
        )
        .route("/assets", get(libraries::assets))
        .route(
            "/sites/{id}/hub",
            put(navigation::set_hub)
                .delete(navigation::unregister_hub)
                .layer(DefaultBodyLimit::max(navigation::MAX_NAVIGATION_BODY)),
        )
        .route("/sites/{id}/joined-hub", put(navigation::join_hub))
        .route(
            "/sites/{id}/navigation",
            get(navigation::site)
                .put(navigation::set_site)
                .layer(DefaultBodyLimit::max(navigation::MAX_NAVIGATION_BODY)),
        )
        .route(
            "/navigation",
            get(navigation::global)
                .put(navigation::set_global)
                .layer(DefaultBodyLimit::max(navigation::MAX_NAVIGATION_BODY)),
        )
        .route(
            "/sites/{id}/look",
            get(looks::show)
                .put(looks::set)
                .layer(DefaultBodyLimit::max(looks::MAX_LOOK_BODY)),
        )
        .route("/sites/{id}/look/settings", get(looks::settings))
        .route("/sites/{id}/look/history", get(looks::history))
        .route("/sites/{id}/look/rollback", post(looks::rollback))
        .route("/sites/{id}/look/copy", post(looks::copy))
        .route("/sites/{id}/font-packages", post(looks::create_package))
        .route(
            "/sites/{id}/font-packages/{package}",
            patch(looks::change_package).delete(looks::delete_package),
        )
        .route("/sites/{id}/themes", post(looks::create_theme))
        .route(
            "/sites/{id}/themes/{theme}",
            patch(looks::change_theme).delete(looks::delete_theme),
        )
        .route("/brand", get(brand::show))
        .route("/brand/themes", post(brand::create_theme))
        .route(
            "/brand/themes/{id}",
            patch(brand::change_theme).delete(brand::delete_theme),
        )
        .route("/brand/images", post(brand::add_image))
        .route(
            "/brand/images/{id}",
            get(brand::image)
                .patch(brand::rename_image)
                .delete(brand::remove_image),
        )
        .route("/brand/managers", put(brand::set_managers))
        .route("/brand/fonts", post(brand::add_font))
        .route(
            "/brand/fonts/{id}",
            get(brand::font).delete(brand::remove_font),
        )
        .route("/brand/font-packages", post(brand::create_package))
        .route(
            "/brand/font-packages/{id}",
            patch(brand::change_package).delete(brand::delete_package),
        )
        .route("/brand/policy", put(brand::set_policy))
        .route("/brand/default-icon", put(brand::set_default_icon))
        .route("/hubs", get(navigation::hubs))
        .route("/hubs/{id}/sites", get(navigation::hub_sites))
        .route("/sites/{id}/template", post(templates::save))
        .route("/templates", get(templates::list))
        .route(
            "/templates/import",
            post(templates::import).layer(DefaultBodyLimit::max(templates::MAX_IMPORT_BODY)),
        )
        .route("/templates/{id}", delete(templates::remove))
        .route("/templates/{id}/package", get(templates::package))
        .route("/sites/{id}/bin", get(bin::list))
        .route("/bin/{id}", delete(bin::remove))
        .route("/bin/{id}/restore", post(bin::restore))
        .route(
            "/bulk",
            post(bulk::run).layer(DefaultBodyLimit::max(bulk::MAX_BODY)),
        )
        .route("/roles", get(roles::list).post(roles::create))
        .route("/roles/{id}", patch(roles::change).delete(roles::remove))
        .route("/groups", get(groups::list).post(groups::create))
        .route("/groups/{id}", get(groups::show).delete(groups::remove))
        .route("/groups/{id}/members", post(groups::add_by_email))
        .route(
            "/groups/{id}/members/{user_id}",
            put(groups::add_member).delete(groups::remove_member),
        )
        .route(
            "/nodes/{id}",
            get(nodes::show).patch(nodes::change).delete(nodes::remove),
        )
        .route(
            "/nodes/{id}/children",
            get(nodes::children).post(nodes::create),
        )
        .route("/nodes/{id}/access", get(nodes::access).put(nodes::share))
        .route(
            "/nodes/{id}/access/{kind}/{principal_id}",
            delete(nodes::unshare),
        )
        .route("/nodes/{id}/inheritance", post(nodes::inheritance))
        .route("/nodes/{id}/copy", post(nodes::copy))
        .route("/nodes/{id}/links", get(links::list).post(links::create))
        .route("/links/{id}", delete(links::revoke))
        .route("/nodes/{id}/requests", post(requests::create))
        .route("/requests", get(requests::open))
        .route("/requests/mine", get(requests::mine))
        .route("/requests/{id}/approve", post(requests::approve))
        .route("/requests/{id}/decline", post(requests::decline))
        .route(
            "/settings/sharing",
            get(settings::show).put(settings::change),
        )
        .route(
            "/settings/regional",
            get(settings::regional).put(settings::change_regional),
        )
        .route(
            "/settings/files",
            get(settings::files).put(settings::change_files),
        )
        .route(
            "/settings/scanner",
            get(settings::scanner).put(settings::change_scanner),
        )
        .nest("/l", crate::links::api_router())
        .route("/nodes/{id}/inheritance/preview", get(explorer::preview))
        .route("/explorer/{root}", get(explorer::breaks))
        .route("/explorer/{root}/counts", get(explorer::counts))
        .route("/explorer/{root}/export.csv", get(explorer::export))
        .route("/docs", post(docs::create))
        .route("/docs/upload", post(content::upload))
        .route("/uploads", post(uploads::start))
        .route(
            "/uploads/{id}",
            get(uploads::show)
                .put(uploads::chunk)
                .delete(uploads::abandon),
        )
        .route("/uploads/{id}/finish", post(uploads::finish))
        .route("/docs/{id}", get(docs::open))
        .route("/docs/{id}/live-base", get(content::live_base))
        .route(
            "/docs/{id}/content",
            get(content::download).put(content::save),
        )
        .route("/docs/{id}/live-checkpoint", put(content::live_checkpoint))
        .route(
            "/docs/{id}/live-checkpoint/hash",
            post(content::live_checkpoint_hash),
        )
        .route("/docs/{id}/preview", get(content::preview))
        .route("/docs/{id}/thumbnail", get(content::thumbnail))
        .route("/docs/{id}/access", get(nodes::access).put(nodes::share))
        .route("/docs/{id}/events", get(docs::events))
        .route(
            "/docs/{id}/access/{kind}/{principal_id}",
            delete(nodes::unshare),
        )
        .route("/admin/users", get(admin::users).post(admin::create))
        .route("/admin/users/{id}/disable", post(admin::disable))
        .route("/admin/users/{id}/enable", post(admin::enable))
        .route(
            "/admin/looks/apply",
            post(admin_looks::apply).layer(DefaultBodyLimit::max(admin_looks::MAX_APPLY_BODY)),
        )
        .route("/admin/sites", get(admin::sites))
        .route("/admin/sites/{id}", delete(admin::purge_site))
        .route("/admin/sites/{id}/restore", post(admin::restore_site))
        .route("/admin/scan-existing", post(quarantine::scan_existing))
        .route("/admin/quarantine", get(quarantine::list))
        .route(
            "/admin/quarantine/{sha256}/release",
            post(quarantine::release),
        )
        .route(
            "/admin/quarantine/{sha256}/delete",
            post(quarantine::delete),
        )
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(require_same_origin))
        .layer(middleware::from_fn(no_store))
}

/// Keeps every `/api` answer, errors included, out of caches, unless it says otherwise itself
/// (a thumbnail, cached privately for a few minutes).
async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Refuses requests from other origins, and requests that change something without `Origin`.
async fn require_same_origin(request: Request, next: Next) -> Response {
    let origin_needed = request.method() != Method::GET;
    if !SameOrigin::of(request.extensions())
        || (origin_needed && !request.headers().contains_key(ORIGIN))
    {
        return ApiError::Forbidden.into_response();
    }
    next.run(request).await
}

/// An API failure, answered as `{"error": "<code>"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// 400: the body is not the JSON expected.
    BadRequest,
    /// 401: no valid session.
    NotSignedIn,
    /// 401: wrong email or password.
    InvalidCredentials,
    /// 403: the request failed the same-origin check.
    Forbidden,
    /// 403: deleting the folder needs `delete` on a place inside it with permissions of its own,
    /// which the caller lacks.
    ForbiddenBelow,
    /// 403: the node's site is archived, read-only for everyone until an administrator
    /// reactivates it. Answered in place of `forbidden` whenever that is why.
    Archived,
    /// 404: no such API route, document or account, or none the caller may see.
    NotFound,
    /// 404: no account has the email given.
    NoSuchUser,
    /// 409: the email is already in use.
    EmailTaken,
    /// 409: a scope must keep someone who can manage access.
    LastManager,
    /// 403: the role has permissions the caller does not hold there.
    BeyondYourAccess,
    /// 409: the role is still assigned somewhere.
    RoleInUse,
    /// 400: a built-in role cannot be changed or deleted.
    BuiltinRole,
    /// 409: the group is still assigned somewhere.
    GroupInUse,
    /// 400: a site's groups cannot be deleted.
    SiteGroup,
    /// 409: another item in the same place already has that name (ignoring case).
    NameTaken,
    /// 409: a folder above the item to restore is in the recycle bin, and the caller may not
    /// restore it.
    ParentInBin,
    /// 400: a folder cannot be moved into itself or below itself.
    IntoItself,
    /// 400: that kind of item cannot go there, or cannot be moved or deleted.
    WrongPlace,
    /// 400: a site cannot be deleted, and cannot inherit.
    IsSite,
    /// 409: the site is deleted; only restoring and purging it are possible.
    SiteDeleted,
    /// 409: the site is not deleted, so it cannot be restored or purged.
    NotDeleted,
    /// 409: the site is not a hub, so it cannot be joined or unregistered.
    NotAHub,
    /// 409: a hub cannot join a hub, and a site that joined a hub cannot become one.
    IsHub,
    /// 409: one administrator must stay enabled.
    LastAdmin,
    /// 422: a well-formed request with a value that makes no sense, such as an unknown role or
    /// an id that is not an id.
    InvalidRequest,
    /// 422: a file-share path is not a safe absolute path.
    InvalidSharePath(&'static str),
    /// 422: a safe path is outside every administrator-approved root.
    ShareNotApproved,
    /// 422: the approved folder cannot be listed.
    ShareUnreachable(&'static str),
    /// 422: the S3 endpoint named is not one an administrator approved (or a built-in
    /// `aws:<region>`).
    S3EndpointNotApproved,
    /// 422: the bucket or the prefix breaks its rules; the reason is `bucket` or `prefix`.
    InvalidS3Place(&'static str),
    /// 422: the probe of the bucket failed; the reason is `keys`, `bucket`, `denied`, `region`,
    /// `clock`, `address` or `offline`, and the message a plain sentence of ours.
    S3Unreachable(&'static str),
    /// 422: an entry of the approved S3 endpoints breaks a rule: which entry (its place in the
    /// list, none when the list itself is refused) and why.
    InvalidS3Endpoint(Option<usize>, &'static str),
    /// 409: another library uses the same bucket under an overlapping prefix.
    S3PlaceInUse,
    /// 422: not an acceptable document or person name.
    InvalidName,
    /// 422: not an acceptable email address.
    InvalidEmail,
    /// 422: not a well-formed language tag.
    InvalidLanguage,
    /// 422: not a time zone the server knows.
    InvalidTimeZone,
    /// 422: a navigation that breaks one of its rules.
    InvalidNavigation,
    /// 422: a template or template package that breaks one of its rules; the answer's
    /// `reason` says which.
    InvalidTemplate(&'static str),
    /// 422: a library's kind or settings that break one of their rules; the answer's `reason`
    /// says which.
    InvalidLibrarySettings(&'static str),
    /// 409: the library's folders are off: no new folder, and no folder moved or copied in.
    FoldersOff,
    /// 409: a library is connected to a source only while it is empty.
    LibraryNotEmpty,
    /// 409: the library is connected to a source already.
    SourceExists,
    /// 409: another library uses the same or an overlapping file-share location.
    ShareLocationInUse,
    /// 409: changes to the library's store are accepted but not yet written, so it is not
    /// disconnected or made read-only yet.
    SourceWritesPending,
    /// 409: a read-and-write S3 library does not take this action yet (rename, move, copy in,
    /// delete, restore, a blank file, a file from a template).
    SourceActionUnavailable,
    /// 409: a route slice 1 lets through in a read-and-write S3 library, whose write is not
    /// built yet. Answered as `source_action_unavailable`; removed once every such route
    /// writes.
    SourceWriteNotBuilt,
    /// 409: the read-and-write library is read-only until its storage accepts changes again.
    SourceWriteBlocked,
    /// 422: the write probe found that the access key may read the bucket but not write to it.
    S3NotWritable,
    /// 403: only an administrator may make a library read and write.
    ReadWriteAdminOnly,
    /// 409: the library is kept in step with a source (a GitHub repository), so nothing in it
    /// is created, changed, moved, copied in, deleted or restored here; administrators too.
    /// The answer's `message` says so in words.
    SourceReadOnly,
    /// 409: the file share or S3 storage cannot currently supply this file. The reason is a
    /// stable public classification and never operating-system text, a path, a bucket or a key;
    /// the kind of source picks the message's wording.
    SourceUnavailable(&'static str, SourceKind),
    /// 409: Phase 1 cannot copy a file or folder out of a file-share or S3 library.
    SourceCopyUnavailable,
    /// 422: GitHub has no such repository, or the token given (or none) cannot see it; the
    /// answer's `message` says so in words.
    RepositoryNotFound,
    /// 422: GitHub refused the token given; the answer's `message` says so in words.
    TokenRejected,
    /// 422: the repository has no such branch; the answer's `message` says so in words.
    BranchNotFound,
    /// 422: the repository holds more than this server mirrors for one library (its bytes, or
    /// its files below the folder); the answer's `message` says so in words.
    RepositoryTooLarge,
    /// 409: a sync of the library is running, so its connection cannot be removed now; the
    /// answer's `message` says so in words.
    SourceBusy,
    /// 422: the name does not keep its editor's extension, or a file no editor opens would
    /// take an editor's extension.
    WrongExtension,
    /// 422: the file's type is blocked by the organisation or not allowed by its library; the
    /// answer's `extension` names it (empty for none).
    BlockedFileType(FileType),
    /// 413: the file is larger than its library takes; the answer's `limit` is the library's
    /// limit in bytes.
    FileTooLarge(u64),
    /// 413: the file is larger than one write to S3 storage takes; the answer's `limit` is that
    /// limit in bytes and its `message` says so in words.
    SourceFileTooLarge(u64),
    /// 423: the content is held by the malware scanner (waiting for it, failed, too large or
    /// infected), so its bytes are not handed out; the answer's `scan` says which.
    Held(ScanState),
    /// 422: not an ICAP scanner address (`icap://host[:port]/service`, no password).
    InvalidScannerUrl,
    /// 503: too much is being done at once to start this; asking again later may work.
    Busy,
    /// 415: the file is not of a type that has a preview, or a text file is not UTF-8.
    NoPreview,
    /// 415: no thumbnail is made for the file: not a PNG, JPEG, GIF or WebP image, or one that
    /// cannot be decoded within the limits.
    NoThumbnail,
    /// 422: the password is too short or too long.
    WeakPassword,
    /// 413: the body is larger than the route allows (16 KiB; file content, `max_file_bytes`).
    TooLarge,
    /// 415: the body is not `application/json`.
    NotJson,
    /// 429: too many failed sign-ins for this email.
    TooManyAttempts,
    /// 404: a link address that does not work: unknown, revoked, expired, an anyone link while
    /// they are switched off, or one that does not reach what was asked; never distinguished.
    LinkNotFound,
    /// 400: a sharing link's hidden group is changed only by the link code.
    LinkGroup,
    /// 400: a link's role may not manage access or the site, and an anyone link only views.
    LinkRoleTooHigh,
    /// 409: approving would give the requester nothing they do not already have.
    AlreadyHas,
    /// 400: anyone links are switched off for the organisation.
    AnyoneLinksOff,
    /// 400: an anyone link needs an expiry.
    ExpiryRequired,
    /// 400: the expiry is further away than the organisation allows.
    ExpiryTooLong,
    /// 400: the expiry has already passed.
    ExpiryInPast,
    /// 403: production mode, and the proxy received the request over plain HTTP.
    HttpsRequired,
    /// 429: too many requests for access in the last day.
    TooManyRequests,
    /// 409: the request for access was decided already.
    AlreadyDecided,
    /// 400: a request's message or a reply is longer than 500 characters.
    MessageTooLong,
    /// 409: someone holds the editing seat of the document a new version was uploaded for.
    BeingEdited,
    /// 400: a plain-text document takes no uploaded versions; it is edited in its room.
    TextDocument,
    /// 422: the uploaded bytes are not the size and SHA-256 declared.
    ChecksumMismatch,
    /// 429: the person already has the most upload sessions open.
    TooManyUploads,
    /// 400: a chunk's `Content-Range` is missing, malformed, beyond the declared size, or not
    /// the length of its body.
    InvalidRange,
    /// 507: the site has no room left under its storage quota for what would be stored.
    QuotaExceeded,
    /// 422: the file is not a brand image: not PNG, JPEG, WebP, GIF or a safe SVG, judged by
    /// its content.
    NotAnImage,
    /// 413: the image is larger than its slot takes (1 MiB, or 5 MiB for a background), or
    /// more than 8,000 pixels on a side.
    ImageTooLarge,
    /// 409: a brand image's content is no longer stored.
    ImageGone,
    /// 409: there are already as many as allowed; the answer's `limit` says how many.
    TooMany(usize),
    /// 422: the file is not a font: not woff2, woff, ttf or otf, judged by its content.
    NotAFont,
    /// 413: the font file is larger than 2 MiB.
    FontTooLarge,
    /// 422: a font is added only once the organisation's licence for it is confirmed.
    LicenceNotConfirmed,
    /// 422: the image cannot be a tab icon: a png or svg, square within 10 %, a png at least
    /// 32 pixels a side, at most 256 KiB; or a brand image that is not of kind `icon`.
    NotAnIcon,
    /// 503: the database failed.
    Unavailable,
}

impl ApiError {
    /// A database failure: logs `cause` to standard error and answers `503`.
    ///
    /// `cause` must not contain the database URL; [`records::DbError`] does not.
    pub(crate) fn unavailable(cause: &dyn fmt::Display) -> ApiError {
        eprintln!("sharecraft-server: database failure: {cause}");
        ApiError::Unavailable
    }

    /// The code the answer's `error` names.
    pub(crate) fn code(self) -> &'static str {
        self.status_and_code().1
    }

    fn status_and_code(self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::NotSignedIn => (StatusCode::UNAUTHORIZED, "not_signed_in"),
            ApiError::InvalidCredentials => (StatusCode::UNAUTHORIZED, "invalid_credentials"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::ForbiddenBelow => (StatusCode::FORBIDDEN, "forbidden_below"),
            ApiError::Archived => (StatusCode::FORBIDDEN, "archived"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::NoSuchUser => (StatusCode::NOT_FOUND, "no_such_user"),
            ApiError::EmailTaken => (StatusCode::CONFLICT, "email_taken"),
            ApiError::LastManager => (StatusCode::CONFLICT, "last_manager"),
            ApiError::BeyondYourAccess => (StatusCode::FORBIDDEN, "beyond_your_access"),
            ApiError::RoleInUse => (StatusCode::CONFLICT, "role_in_use"),
            ApiError::BuiltinRole => (StatusCode::BAD_REQUEST, "builtin_role"),
            ApiError::GroupInUse => (StatusCode::CONFLICT, "group_in_use"),
            ApiError::SiteGroup => (StatusCode::BAD_REQUEST, "site_group"),
            ApiError::NameTaken => (StatusCode::CONFLICT, "name_taken"),
            ApiError::ParentInBin => (StatusCode::CONFLICT, "parent_in_bin"),
            ApiError::IntoItself => (StatusCode::BAD_REQUEST, "into_itself"),
            ApiError::WrongPlace => (StatusCode::BAD_REQUEST, "wrong_place"),
            ApiError::IsSite => (StatusCode::BAD_REQUEST, "is_site"),
            ApiError::SiteDeleted => (StatusCode::CONFLICT, "site_deleted"),
            ApiError::NotDeleted => (StatusCode::CONFLICT, "not_deleted"),
            ApiError::NotAHub => (StatusCode::CONFLICT, "not_a_hub"),
            ApiError::IsHub => (StatusCode::CONFLICT, "is_hub"),
            ApiError::LastAdmin => (StatusCode::CONFLICT, "last_admin"),
            ApiError::InvalidRequest => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"),
            ApiError::InvalidSharePath(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_share_path")
            }
            ApiError::ShareNotApproved => (StatusCode::UNPROCESSABLE_ENTITY, "share_not_approved"),
            ApiError::ShareUnreachable(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "share_unreachable")
            }
            ApiError::S3EndpointNotApproved => {
                (StatusCode::UNPROCESSABLE_ENTITY, "s3_endpoint_not_approved")
            }
            ApiError::InvalidS3Place(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_s3_place"),
            ApiError::S3Unreachable(_) => (StatusCode::UNPROCESSABLE_ENTITY, "s3_unreachable"),
            ApiError::InvalidS3Endpoint(..) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_s3_endpoint")
            }
            ApiError::S3PlaceInUse => (StatusCode::CONFLICT, "s3_place_in_use"),
            ApiError::InvalidName => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_name"),
            ApiError::InvalidEmail => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_email"),
            ApiError::InvalidLanguage => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_language"),
            ApiError::InvalidTimeZone => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_time_zone"),
            ApiError::InvalidNavigation => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_navigation"),
            ApiError::InvalidTemplate(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_template"),
            ApiError::InvalidLibrarySettings(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_library_settings")
            }
            ApiError::FoldersOff => (StatusCode::CONFLICT, "folders_off"),
            ApiError::LibraryNotEmpty => (StatusCode::CONFLICT, "library_not_empty"),
            ApiError::SourceExists => (StatusCode::CONFLICT, "source_exists"),
            ApiError::ShareLocationInUse => (StatusCode::CONFLICT, "share_location_in_use"),
            ApiError::SourceWritesPending => (StatusCode::CONFLICT, "source_writes_pending"),
            ApiError::SourceActionUnavailable | ApiError::SourceWriteNotBuilt => {
                (StatusCode::CONFLICT, "source_action_unavailable")
            }
            ApiError::SourceWriteBlocked => (StatusCode::CONFLICT, "source_write_blocked"),
            ApiError::S3NotWritable => (StatusCode::UNPROCESSABLE_ENTITY, "s3_not_writable"),
            ApiError::ReadWriteAdminOnly => (StatusCode::FORBIDDEN, "read_write_admin_only"),
            ApiError::SourceReadOnly => (StatusCode::CONFLICT, "source_read_only"),
            ApiError::SourceUnavailable("offline", _) => {
                (StatusCode::SERVICE_UNAVAILABLE, "source_unavailable")
            }
            ApiError::SourceUnavailable(..) => (StatusCode::CONFLICT, "source_unavailable"),
            ApiError::SourceCopyUnavailable => (StatusCode::CONFLICT, "source_copy_unavailable"),
            ApiError::RepositoryNotFound => {
                (StatusCode::UNPROCESSABLE_ENTITY, "repository_not_found")
            }
            ApiError::TokenRejected => (StatusCode::UNPROCESSABLE_ENTITY, "token_rejected"),
            ApiError::BranchNotFound => (StatusCode::UNPROCESSABLE_ENTITY, "branch_not_found"),
            ApiError::SourceBusy => (StatusCode::CONFLICT, "source_busy"),
            ApiError::RepositoryTooLarge => {
                (StatusCode::UNPROCESSABLE_ENTITY, "repository_too_large")
            }
            ApiError::WrongExtension => (StatusCode::UNPROCESSABLE_ENTITY, "wrong_extension"),
            ApiError::BlockedFileType(_) => (StatusCode::UNPROCESSABLE_ENTITY, "blocked_file_type"),
            ApiError::FileTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "file_too_large"),
            ApiError::SourceFileTooLarge(_) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "source_file_too_large")
            }
            ApiError::Held(_) => (StatusCode::LOCKED, "held"),
            ApiError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
            ApiError::NoPreview => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_preview"),
            ApiError::NoThumbnail => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_thumbnail"),
            ApiError::InvalidScannerUrl => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_scanner_url")
            }
            ApiError::WeakPassword => (StatusCode::UNPROCESSABLE_ENTITY, "weak_password"),
            ApiError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            ApiError::NotJson => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not_json"),
            ApiError::TooManyAttempts => (StatusCode::TOO_MANY_REQUESTS, "too_many_attempts"),
            ApiError::LinkNotFound => (StatusCode::NOT_FOUND, "link_not_found"),
            ApiError::LinkGroup => (StatusCode::BAD_REQUEST, "link_group"),
            ApiError::LinkRoleTooHigh => (StatusCode::BAD_REQUEST, "link_role_too_high"),
            ApiError::AlreadyHas => (StatusCode::CONFLICT, "already_has"),
            ApiError::AnyoneLinksOff => (StatusCode::BAD_REQUEST, "anyone_links_off"),
            ApiError::ExpiryRequired => (StatusCode::BAD_REQUEST, "expiry_required"),
            ApiError::ExpiryTooLong => (StatusCode::BAD_REQUEST, "expiry_too_long"),
            ApiError::ExpiryInPast => (StatusCode::BAD_REQUEST, "expiry_in_past"),
            ApiError::HttpsRequired => (StatusCode::FORBIDDEN, "https_required"),
            ApiError::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, "too_many_requests"),
            ApiError::AlreadyDecided => (StatusCode::CONFLICT, "already_decided"),
            ApiError::MessageTooLong => (StatusCode::BAD_REQUEST, "message_too_long"),
            ApiError::BeingEdited => (StatusCode::CONFLICT, "being_edited"),
            ApiError::TextDocument => (StatusCode::BAD_REQUEST, "text_document"),
            ApiError::ChecksumMismatch => (StatusCode::UNPROCESSABLE_ENTITY, "checksum_mismatch"),
            ApiError::TooManyUploads => (StatusCode::TOO_MANY_REQUESTS, "too_many_uploads"),
            ApiError::InvalidRange => (StatusCode::BAD_REQUEST, "invalid_range"),
            ApiError::QuotaExceeded => (StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded"),
            ApiError::NotAnImage => (StatusCode::UNPROCESSABLE_ENTITY, "not_an_image"),
            ApiError::ImageTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "image_too_large"),
            ApiError::ImageGone => (StatusCode::CONFLICT, "image_gone"),
            ApiError::TooMany(_) => (StatusCode::CONFLICT, "too_many"),
            ApiError::NotAFont => (StatusCode::UNPROCESSABLE_ENTITY, "not_a_font"),
            ApiError::FontTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "font_too_large"),
            ApiError::LicenceNotConfirmed => {
                (StatusCode::UNPROCESSABLE_ENTITY, "licence_not_confirmed")
            }
            ApiError::NotAnIcon => (StatusCode::UNPROCESSABLE_ENTITY, "not_an_icon"),
            ApiError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        }
    }
}

impl From<AccessError> for ApiError {
    fn from(e: AccessError) -> Self {
        match e {
            AccessError::NotFound => ApiError::NotFound,
            AccessError::NoSuchUser => ApiError::NoSuchUser,
            AccessError::NoSuchGroup => ApiError::NotFound,
            // As an unknown role name was before roles could be defined.
            AccessError::NoSuchRole => ApiError::InvalidRequest,
            AccessError::Forbidden => ApiError::Forbidden,
            AccessError::ForbiddenBelow => ApiError::ForbiddenBelow,
            AccessError::Archived => ApiError::Archived,
            AccessError::LastManager => ApiError::LastManager,
            AccessError::BeyondYourAccess => ApiError::BeyondYourAccess,
            AccessError::RoleInUse => ApiError::RoleInUse,
            AccessError::BuiltinRole => ApiError::BuiltinRole,
            AccessError::GroupInUse => ApiError::GroupInUse,
            AccessError::SiteGroup => ApiError::SiteGroup,
            AccessError::LinkGroup => ApiError::LinkGroup,
            AccessError::LinkRoleTooHigh => ApiError::LinkRoleTooHigh,
            AccessError::AnyoneLinksOff => ApiError::AnyoneLinksOff,
            AccessError::ExpiryRequired => ApiError::ExpiryRequired,
            AccessError::ExpiryTooLong => ApiError::ExpiryTooLong,
            AccessError::ExpiryInPast => ApiError::ExpiryInPast,
            AccessError::AlreadyDecided => ApiError::AlreadyDecided,
            AccessError::AlreadyHas => ApiError::AlreadyHas,
            AccessError::MessageTooLong => ApiError::MessageTooLong,
            // A value that makes no sense for the setting, as any other such value.
            AccessError::InvalidSetting => ApiError::InvalidRequest,
            AccessError::InvalidName => ApiError::InvalidName,
            AccessError::InvalidLanguage => ApiError::InvalidLanguage,
            AccessError::InvalidTimeZone => ApiError::InvalidTimeZone,
            AccessError::InvalidNavigation => ApiError::InvalidNavigation,
            AccessError::InvalidTemplate(reason) => ApiError::InvalidTemplate(reason),
            AccessError::InvalidLibrarySettings(reason) => ApiError::InvalidLibrarySettings(reason),
            AccessError::FoldersOff => ApiError::FoldersOff,
            AccessError::LibraryNotEmpty => ApiError::LibraryNotEmpty,
            AccessError::SourceExists => ApiError::SourceExists,
            AccessError::ShareLocationInUse => ApiError::ShareLocationInUse,
            AccessError::WritesPending => ApiError::SourceWritesPending,
            AccessError::LibraryReadOnly => ApiError::SourceReadOnly,
            AccessError::S3EndpointNotApproved => ApiError::S3EndpointNotApproved,
            AccessError::BlockedFileType(file_type) => ApiError::BlockedFileType(file_type),
            AccessError::FileTooLarge(limit) => ApiError::FileTooLarge(limit),
            AccessError::Held(scan) => ApiError::Held(scan),
            AccessError::InvalidScannerUrl => ApiError::InvalidScannerUrl,
            // `POST /api/docs` answers this itself, naming the extension expected.
            AccessError::WrongExtension => ApiError::InvalidRequest,
            AccessError::NameTaken => ApiError::NameTaken,
            AccessError::WrongPlace => ApiError::WrongPlace,
            AccessError::IntoItself => ApiError::IntoItself,
            AccessError::ParentInBin => ApiError::ParentInBin,
            AccessError::IsSite => ApiError::IsSite,
            AccessError::SiteDeleted => ApiError::SiteDeleted,
            AccessError::NotDeleted => ApiError::NotDeleted,
            AccessError::NotAHub => ApiError::NotAHub,
            AccessError::IsHub => ApiError::IsHub,
            AccessError::QuotaExceeded => ApiError::QuotaExceeded,
            AccessError::ImageGone => ApiError::ImageGone,
            // The server picks a fresh random id, so a clash is not the caller's doing.
            AccessError::Exists
            | AccessError::Db(_)
            | AccessError::Unjudged(_)
            | AccessError::CommitOutcomeUnknown(_) => ApiError::unavailable(&e),
            // The tree is being repaired; a later attempt can succeed.
            AccessError::OutOfStep => ApiError::Unavailable,
        }
    }
}

impl From<LinkError> for ApiError {
    fn from(e: LinkError) -> Self {
        match e {
            LinkError::NotFound => ApiError::LinkNotFound,
            LinkError::Archived => ApiError::Archived,
            LinkError::Db(_) => ApiError::unavailable(&e),
        }
    }
}

impl From<RequestError> for ApiError {
    fn from(e: RequestError) -> Self {
        match e {
            RequestError::TooMany => ApiError::TooManyRequests,
            RequestError::MessageTooLong => ApiError::MessageTooLong,
            RequestError::Archived => ApiError::Archived,
            RequestError::Db(_) => ApiError::unavailable(&e),
        }
    }
}

impl From<AccountError> for ApiError {
    fn from(e: AccountError) -> Self {
        match e {
            AccountError::EmailTaken => ApiError::EmailTaken,
            AccountError::InvalidEmail => ApiError::InvalidEmail,
            AccountError::InvalidName => ApiError::InvalidName,
            AccountError::WeakPassword => ApiError::WeakPassword,
            AccountError::NotFound => ApiError::NotFound,
            AccountError::Forbidden => ApiError::Forbidden,
            AccountError::LastAdmin => ApiError::LastAdmin,
            AccountError::Db(_) => ApiError::unavailable(&e),
        }
    }
}

/// An id from a path, or `422 invalid_request` when it is not one.
fn parse_id(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::InvalidRequest)
}

/// Runs `change` (a database write and the hook that tells the rooms about it, or a save) on a
/// task of its own and waits for it. A request whose caller goes away is dropped at its next
/// `.await`; the task is not, so a write that was stored always reaches the rooms. A task that
/// fails to finish answers `503`.
pub(crate) async fn to_completion<T: Send + 'static>(
    change: impl Future<Output = Result<T, ApiError>> + Send + 'static,
) -> Result<T, ApiError> {
    match tokio::spawn(change).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("sharecraft-server: a change did not finish: {e}");
            Err(ApiError::Unavailable)
        }
    }
}

/// Called once a change that can alter access at or below `node` is stored: an assignment,
/// a break or restore of inheritance on `node`, or a move of `node`. Rechecks (see
/// [`recheck`]) every open document at or below `node`; if it cannot be told whether a
/// document is below `node`, it is checked anyway.
///
/// Callers run it inside [`to_completion`], so a cancelled request still revokes.
pub(crate) async fn recheck_below(state: &AppState, node: NodeId) {
    let db = state.db();
    let mut below = Vec::new();
    for doc in state.rooms().open_documents() {
        match db.is_at_or_below(doc.into(), node).await {
            Ok(true) => below.push(doc),
            Ok(false) => {}
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                below.push(doc);
            }
        }
    }
    recheck(state, below, None).await;
}

/// Called once a change of `user`'s memberships is stored: rechecks (see [`recheck`]) `user`
/// in every open document, wherever they are connected.
///
/// Callers run it inside [`to_completion`], so a cancelled request still revokes.
pub(crate) async fn recheck_person(state: &AppState, user: UserId) {
    let open = state.rooms().open_documents();
    recheck(state, open, Some(user)).await;
}

/// Called once a change of a role's permissions is stored: the role may be held anywhere, so
/// everyone in every open document is rechecked (see [`recheck`]).
///
/// Callers run it inside [`to_completion`], so a cancelled request still revokes.
pub(crate) async fn recheck_everyone(state: &AppState) {
    let open = state.rooms().open_documents();
    recheck(state, open, None).await;
}

/// For each of `docs`, one at a time in id order, reads the access of each person connected
/// to it (only of `only`, when given) afresh (so an administrator who loses a grant keeps every
/// permission) and tells the room: connections that lost access are removed, the others get
/// their new rights. If access cannot be read, that person's connections are removed, as it
/// can no longer be confirmed.
///
/// Each document's readings and messages happen under its access lock, so that of two changes
/// made at once the room hears the later reading last. One lock is held at a time.
async fn recheck(state: &AppState, mut docs: Vec<DocId>, only: Option<UserId>) {
    let rooms = state.rooms();
    let db = state.db();
    docs.sort_by_key(|doc| doc.0);
    docs.dedup();
    for doc in docs {
        let _in_order = rooms.order_access(doc).await;
        // A file of a connected library stays read-only whatever role is read (one query per
        // document; a failure to tell counts as connected).
        let connected = match db.source_library_of(doc.into()).await {
            Ok(library) => library.is_some(),
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                true
            }
        };
        for user in rooms.connected_users(doc).await {
            if only.is_some_and(|only| only != user) {
                continue;
            }
            let access = match db.effective_access(user, doc.into()).await {
                Ok(access) => access,
                Err(e) => {
                    eprintln!("sharecraft-server: database failure: {e}");
                    None
                }
            };
            let access = if connected {
                access.map(nodes::read_only_seat)
            } else {
                access
            };
            rooms.send_to(doc, AccessChange::SetAccess { user, role: access });
        }
    }
}

/// Called after `user`'s account was disabled: their connections to every document are removed.
/// Removal is final, so it needs no ordering with other access changes.
fn after_account_disabled(state: &AppState, user: UserId) {
    state.rooms().send_all(|| AccessChange::RemoveUser { user });
}

/// Called after `session` was ended by its owner signing out: that session's connections are
/// removed; the same person's other sign-ins stay.
fn after_sign_out(state: &AppState, session: SessionId) {
    state
        .rooms()
        .send_all(|| AccessChange::EndSession { session });
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = match self {
            ApiError::InvalidSharePath(reason) => json!({
                "error": code,
                "reason": reason,
                "message": share_path_message(reason),
            }),
            ApiError::ShareUnreachable(reason) => json!({
                "error": code,
                "reason": reason,
                "message": share_unreachable_message(reason),
            }),
            ApiError::InvalidS3Place(reason) => json!({
                "error": code,
                "reason": reason,
                "message": if reason == "prefix" {
                    "The prefix must be empty or folder names separated by \"/\", with no empty, \".\" or \"..\" name."
                } else {
                    "The bucket name is not one S3 allows: 3 to 63 lower-case letters, digits, dots and hyphens."
                },
            }),
            ApiError::S3Unreachable(reason) => json!({
                "error": code,
                "reason": reason,
                "message": s3_unreachable_message(reason),
            }),
            ApiError::InvalidS3Endpoint(entry, reason) => json!({
                "error": code,
                "entry": entry,
                "reason": reason,
            }),
            ApiError::InvalidTemplate(reason) | ApiError::InvalidLibrarySettings(reason) => {
                json!({ "error": code, "reason": reason })
            }
            ApiError::BlockedFileType(file_type) => {
                json!({ "error": code, "extension": file_type })
            }
            ApiError::FileTooLarge(limit) => json!({ "error": code, "limit": limit }),
            ApiError::SourceFileTooLarge(limit) => json!({
                "error": code,
                "limit": limit,
                "message": source_file_too_large_message(limit),
            }),
            ApiError::Held(scan) => json!({ "error": code, "scan": scan }),
            ApiError::TooMany(limit) => json!({ "error": code, "limit": limit }),
            ApiError::SourceReadOnly => json!({ "error": code, "message": SOURCE_READ_ONLY }),
            ApiError::SourceWritesPending => {
                json!({ "error": code, "message": SOURCE_WRITES_PENDING })
            }
            ApiError::SourceActionUnavailable | ApiError::SourceWriteNotBuilt => {
                json!({ "error": code, "message": SOURCE_ACTION_UNAVAILABLE })
            }
            ApiError::SourceWriteBlocked => {
                json!({ "error": code, "message": SOURCE_WRITE_BLOCKED })
            }
            ApiError::S3NotWritable => json!({ "error": code, "message": S3_NOT_WRITABLE }),
            ApiError::ReadWriteAdminOnly => {
                json!({ "error": code, "message": READ_WRITE_ADMIN_ONLY })
            }
            ApiError::SourceUnavailable(reason, kind) => json!({
                "error": code,
                "reason": reason,
                "message": source_unavailable_message(reason, kind),
            }),
            ApiError::SourceCopyUnavailable => {
                json!({ "error": code, "message": SOURCE_COPY_UNAVAILABLE })
            }
            ApiError::RepositoryNotFound => json!({
                "error": code,
                "message": "The repository was not found, or the token cannot see it.",
            }),
            ApiError::TokenRejected => json!({
                "error": code,
                "message": "GitHub refused the token: it is wrong, expired or revoked, or cannot read this repository.",
            }),
            ApiError::SourceBusy => json!({
                "error": code,
                "message": "A sync is running; try again in a moment.",
            }),
            ApiError::BranchNotFound => json!({
                "error": code,
                "message": "The repository has no such branch.",
            }),
            ApiError::RepositoryTooLarge => json!({
                "error": code,
                "message": "The repository holds more than this server mirrors for one library.",
            }),
            _ => json!({ "error": code }),
        };
        (status, Json(body)).into_response()
    }
}

/// The plain sentence for a file over the limit of one write to S3 storage: `limit` is said
/// in the largest unit that holds it exactly (`512 MiB`, `2 KiB`, or bytes).
fn source_file_too_large_message(limit: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    let words = if limit >= GIB && limit.is_multiple_of(GIB) {
        format!("{} GiB", limit / GIB)
    } else if limit >= MIB && limit.is_multiple_of(MIB) {
        format!("{} MiB", limit / MIB)
    } else if limit >= KIB && limit.is_multiple_of(KIB) {
        format!("{} KiB", limit / KIB)
    } else {
        format!("{limit} bytes")
    };
    format!("Files over {words} cannot be saved to S3 storage yet.")
}

/// The plain sentence for a failed probe of a bucket. Never the store's own text.
fn s3_unreachable_message(reason: &str) -> &'static str {
    match reason {
        "keys" => "The S3 storage did not accept the access key and secret.",
        "bucket" => "The S3 storage has no bucket of that name.",
        "denied" => "The access key is not allowed to list this bucket or prefix.",
        "region" => "The bucket is in another region than the endpoint's.",
        "clock" => "This server's clock differs too much from the S3 storage's.",
        "address" => "The endpoint's address is not one this server may reach.",
        _ => "The S3 storage could not be reached. Try again later.",
    }
}

fn share_path_message(reason: &str) -> &'static str {
    match reason {
        "relative" => "Enter an absolute server or network path.",
        "traversal" => "The path cannot contain a parent-folder step.",
        "device" => "Device paths cannot be used as file shares.",
        "stream" => "Alternate data streams cannot be used as file shares.",
        "too_long" => "The file-share path is too long.",
        "whole_drive" => "Approve a folder rather than a whole drive or administrative share.",
        _ => "The file-share path contains a name that cannot be used.",
    }
}

fn share_unreachable_message(reason: &str) -> &'static str {
    match reason {
        "not_found" => "The file-share folder was not found.",
        "denied" => "The server is not allowed to list the file-share folder.",
        _ => "The file-share folder is unavailable. Try again later.",
    }
}

/// The plain sentence for an unavailable external file: the file-share wording, or for an S3
/// library the S3 wording. `archived` and `budget` are the same for both.
fn source_unavailable_message(reason: &str, kind: SourceKind) -> &'static str {
    if kind == SourceKind::S3 {
        return match reason {
            "gone" => "This file is no longer in the library's S3 storage.",
            "denied" => "The library's S3 access key is not allowed to read this file.",
            "offline" => {
                "The library's S3 storage cannot be reached. Files opened before still open; others will open when it is back."
            }
            "changing" => {
                "This file changed in S3 storage while it was being read. Try again in a moment."
            }
            "too_large" => {
                "This file is too large to open from S3 storage with the current cache size."
            }
            "not_approved" => {
                "The S3 endpoint this library uses is no longer approved by an administrator."
            }
            "archived" => {
                "This file is in archive storage and must be restored there before it can be opened."
            }
            "budget" => {
                "This library has used its request budget for the month. Files opened before still open; a library owner can raise the budget."
            }
            _ => "This file cannot be read from S3 storage right now.",
        };
    }
    match reason {
        "gone" => "The file is no longer available on the file share.",
        "denied" => "The server is not allowed to read this file from the file share.",
        "offline" => "The file share is unavailable. Try again later.",
        "changing" => "The file changed while it was being read. Try again.",
        "too_large" => "The file is too large to cache from the file share.",
        "not_approved" => "The file share location is no longer approved.",
        "archived" => {
            "This file is in archive storage and must be restored there before it can be opened."
        }
        "budget" => {
            "This library has used its request budget for the month. Files opened before still open; a library owner can raise the budget."
        }
        _ => "The file is unavailable from the file share.",
    }
}

/// A JSON request body, refused with an [`ApiError`] when it is not `application/json`, is
/// larger than 16 KiB or does not parse.
pub(crate) struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => Err(match rejection.status() {
                StatusCode::PAYLOAD_TOO_LARGE => ApiError::TooLarge,
                StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::NotJson,
                _ => ApiError::BadRequest,
            }),
        }
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
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn the_size_limit_is_said_in_words() {
        use super::source_file_too_large_message as words;
        assert_eq!(
            words(512 * 1024 * 1024),
            "Files over 512 MiB cannot be saved to S3 storage yet."
        );
        assert_eq!(
            words(2048),
            "Files over 2 KiB cannot be saved to S3 storage yet."
        );
        assert_eq!(
            words(10),
            "Files over 10 bytes cannot be saved to S3 storage yet."
        );
    }

    use crate::members::builtin_access;
    use collab_core::ids::{ClientId, DocId};
    use collab_text::TextOp;
    use protocol::ServerMsg;
    use records::test_support::TestDb;
    use records::{Editor, NewUser, NodeKind, Principal};
    use revisions::RevisionStore;
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::config::{Config, DatabaseUrl};
    use crate::room::{Command, JoinReply, OUTBOUND_CAPACITY, Outbound};
    use crate::tree::{self, TreeOp};

    async fn account(db: &records::Db, name: &str) -> UserId {
        user(db, name, false).await.id
    }

    async fn user(db: &records::Db, name: &str, is_admin: bool) -> records::User {
        db.create_user(
            None,
            NewUser {
                email: &format!("{name}@test.invalid"),
                name,
                password: "a long enough password",
                is_admin,
            },
        )
        .await
        .unwrap()
    }

    /// A text document named Plan.txt in the library of a new site, which an administrator
    /// made; `owner` owns the document.
    async fn placed_document(state: &AppState, owner: UserId) -> DocId {
        let db = state.db();
        let root = user(db, "Root", true).await;
        let site = tree::apply(
            state,
            &root,
            TreeOp::Create {
                parent: None,
                kind: NodeKind::Site,
                name: "Site".into(),
                id: NodeId(Uuid::new_v4()),
            },
        )
        .await
        .unwrap()
        .node;
        let (children, _) = db
            .children(root.id, site.id, None, 10)
            .await
            .unwrap()
            .unwrap();
        let library = children[0].node.id;
        let doc = DocId(Uuid::new_v4());
        tree::create_document(state, &root, library, doc, "Plan.txt", Editor::Text, None)
            .await
            .unwrap();
        db.set_access(root.id, doc, Principal::User(owner), Some("owner"))
            .await
            .unwrap();
        doc
    }

    /// A server's state on `test`'s database, storing documents under `dir`; no listener.
    fn state_for(test: &TestDb, dir: &std::path::Path) -> AppState {
        let config = Config {
            addr: "127.0.0.1:0".parse().unwrap(),
            deployment: Default::default(),
            data_dir: dir.to_path_buf(),
            web_dir: dir.join("web"),
            checkpoint_idle: Duration::from_secs(600),
            checkpoint_max: Duration::from_secs(600),
            room_grace: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
            live_editors: Vec::new(),
            live_heartbeat: Duration::from_secs(60),
            live_checkpoint_quiet: Duration::from_secs(15),
            live_checkpoint_wait: Duration::from_secs(120),
            live_ticket_lifetime: Duration::from_secs(120),
            live_limits: Default::default(),
            database_url: DatabaseUrl::new(test.url()),
            editors_dir: dir.join("editors"),
            max_file_bytes: 64 << 20,
            upload_idle: Duration::from_secs(60),
            github_api: crate::config::DEFAULT_GITHUB_API.to_owned(),
            source_create_batch_size: crate::config::DEFAULT_SOURCE_CREATE_BATCH_SIZE,
            s3_plain_loopback: false,
        };
        let store = RevisionStore::open(&crate::library_path(dir)).unwrap();
        let blobs = revisions::BlobStore::open(&crate::blobs_path(dir)).unwrap();
        AppState::new(config, Arc::new(store), blobs, test.db.clone()).unwrap()
    }

    /// The owner's request to remove Bob's grant is cancelled (its future dropped) after the
    /// grant is deleted but while its hook waits to tell the room. Bob must still be removed.
    #[tokio::test]
    async fn cancelled_revoke_still_removes_the_person() {
        let test = TestDb::create().await;
        let db = test.db.clone();
        let dir = tempfile::tempdir().unwrap();
        let state = state_for(&test, dir.path());
        let ann = account(&db, "Ann").await;
        let bob = account(&db, "Bob").await;
        let doc = placed_document(&state, ann).await;
        db.set_access(ann, doc, Principal::User(bob), Some("editor"))
            .await
            .unwrap();
        let room = state.open_room(doc, "Plan.txt".into());
        let (tx, mut out) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, reply) = oneshot::channel();
        room.commands
            .send(Command::Join {
                conn: 1,
                client: ClientId(Uuid::new_v4()),
                user: bob,
                session: SessionId(Uuid::new_v4()),
                name: "Bob".into(),
                role: builtin_access("editor"),
                resume: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(reply.await.unwrap(), JoinReply::Accepted);

        // Held so that the hook waits after the grant is deleted.
        let held = state.rooms().order_access(doc).await;
        let mut request = Box::pin(nodes::change_access(
            state.clone(),
            ann,
            doc.into(),
            Principal::User(bob),
            None,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                _ = &mut request => panic!("the request finished while the hook could not run"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            if db.doc_access(bob, doc).await.unwrap().is_none() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the grant was never deleted"
            );
        }
        // The caller goes away.
        drop(request);
        drop(held);

        let removed = encode_removed(protocol::RemovedReason::AccessRevoked);
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), out.recv())
                .await
                .expect("Bob was not removed");
            let frames = match next {
                Some(Outbound::One(f)) => vec![f],
                Some(Outbound::Many(fs)) => fs,
                None => panic!("Bob's connection ended without Removed"),
            };
            if frames.iter().any(|f| f.as_str() == removed) {
                break;
            }
        }
    }

    fn encode_removed(reason: protocol::RemovedReason) -> String {
        serde_json::to_string(&ServerMsg::<TextOp>::Removed { reason }).unwrap()
    }

    /// Two changes to Bob's role are stored one after the other, but the hook of the first
    /// is slower: it has read the role and not yet told the room when the second change is
    /// stored and its hook runs. The room must end with the role stored last.
    #[tokio::test]
    async fn access_changes_reach_the_room_in_database_order() {
        let test = TestDb::create().await;
        let db = test.db.clone();
        let dir = tempfile::tempdir().unwrap();
        let state = state_for(&test, dir.path());
        let ann = account(&db, "Ann").await;
        let bob = account(&db, "Bob").await;
        let doc = placed_document(&state, ann).await;
        db.set_access(ann, doc, Principal::User(bob), Some("viewer"))
            .await
            .unwrap();

        // Bob is in the room as a viewer.
        let room = state.open_room(doc, "Plan.txt".into());
        let client = ClientId(Uuid::new_v4());
        let (tx, mut out) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, reply) = oneshot::channel();
        room.commands
            .send(Command::Join {
                conn: 1,
                client,
                user: bob,
                session: SessionId(Uuid::new_v4()),
                name: "Bob".into(),
                role: builtin_access("viewer"),
                resume: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(reply.await.unwrap(), JoinReply::Accepted);

        // The first change, made the way `recheck_below` makes it, paused between
        // reading the role and telling the room.
        let first = state.rooms().order_access(doc).await;
        db.set_access(ann, doc, Principal::User(bob), Some("editor"))
            .await
            .unwrap();
        let first_read = db.doc_access(bob, doc).await.unwrap();
        assert_eq!(first_read, Some(builtin_access("editor")));

        // The second change is stored and its hook runs meanwhile.
        db.set_access(ann, doc, Principal::User(bob), Some("viewer"))
            .await
            .unwrap();
        let mut second = tokio::spawn({
            let state = state.clone();
            async move { recheck_below(&state, doc.into()).await }
        });
        // Long enough for the second hook to finish if nothing holds it back.
        let finished_early = tokio::time::timeout(Duration::from_millis(500), &mut second)
            .await
            .is_ok();

        // The first hook carries on.
        state.rooms().send_to(
            doc,
            AccessChange::SetAccess {
                user: bob,
                role: first_read,
            },
        );
        drop(first);
        if !finished_early {
            second.await.unwrap();
        }

        // A cursor move after both changes marks where they end in Bob's frames.
        room.commands
            .send(Command::Cursor {
                conn: 1,
                client,
                at: Some(1),
            })
            .await
            .unwrap();
        let mut can_edit = None;
        let mut listed = None;
        loop {
            let frames = match tokio::time::timeout(Duration::from_secs(5), out.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Outbound::One(f) => vec![f],
                Outbound::Many(fs) => fs,
            };
            let mut marked = false;
            for frame in frames {
                match serde_json::from_str::<ServerMsg<TextOp>>(frame.as_str()).unwrap() {
                    ServerMsg::Joined { can_edit: c, .. } | ServerMsg::Access { can_edit: c } => {
                        can_edit = Some(c);
                    }
                    ServerMsg::People { participants } => {
                        if let Some(p) = participants.iter().find(|p| p.client == client) {
                            listed = Some(p.can_edit);
                            marked |= p.cursor == Some(1);
                        }
                    }
                    _ => {}
                }
            }
            if marked {
                break;
            }
        }
        let stored = db.doc_access(bob, doc).await.unwrap().unwrap();
        assert_eq!(stored, builtin_access("viewer"));
        assert_eq!(
            can_edit,
            Some(stored.can_edit()),
            "Bob's rights in the room"
        );
        assert_eq!(listed, Some(stored.can_edit()), "Bob in the people list");
        assert!(!finished_early, "the later change was not held back");
    }
}
