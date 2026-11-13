//! Sharing links, for the people who manage access to an item: list an item's links, make one
//! (its address is answered once, never again), and revoke one.
//!
//! The pages and routes a link's address reaches are served by [`crate::links`].

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::HOST;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use records::{Link, LinkKind, LinkToken, RoleRef};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::explorer::UserOut;
use super::nodes::{node_id, rfc3339};
use super::{ApiError, JsonBody, parse_id, recheck_below, to_completion};
use crate::AppState;
use crate::auth::Signed;

/// Who revoked a link, when and why, as answered.
#[derive(Serialize)]
pub(crate) struct RevokedOut {
    /// `None` for the server, as when the link expired.
    by: Option<UserOut>,
    at: Option<String>,
    reason: String,
}

/// A link as answered: never its token, nor its hidden group.
#[derive(Serialize)]
pub(crate) struct LinkOut {
    id: String,
    /// The shared item.
    node: String,
    kind: LinkKind,
    /// `None` only for a revoked link whose role has since been deleted.
    role: Option<RoleRef>,
    created_by: UserOut,
    created_at: Option<String>,
    expires_at: Option<String>,
    revoked: Option<RevokedOut>,
    /// How many people have joined it.
    members: i64,
}

impl From<Link> for LinkOut {
    fn from(link: Link) -> Self {
        LinkOut {
            id: link.id.to_string(),
            node: link.node.0.to_string(),
            kind: link.kind,
            role: link.role,
            created_by: (&link.created_by).into(),
            created_at: rfc3339(Some(link.created_at)),
            expires_at: rfc3339(link.expires_at),
            revoked: link.revoked.map(|(by, at, reason)| RevokedOut {
                by: by.as_ref().map(UserOut::from),
                at: rfc3339(Some(at)),
                reason,
            }),
            members: link.members,
        }
    }
}

/// `POST /api/nodes/{id}/links`: a new link with its address.
#[derive(Serialize)]
pub(crate) struct NewLinkOut {
    #[serde(flatten)]
    link: LinkOut,
    /// `http://<the address the caller is using>/l/<token>`. Shown only now: the token is not
    /// stored.
    url: String,
}

/// `GET /api/nodes/{id}/links`: every link made on the node, live or revoked, newest first.
/// Needs `manage_access` on the node.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Vec<LinkOut>>, ApiError> {
    let node = node_id(&id)?;
    let links = state.db().links_on(signed.session.user.id, node).await?;
    Ok(Json(links.into_iter().map(LinkOut::from).collect()))
}

/// The body of `POST /api/nodes/{id}/links`.
#[derive(Deserialize)]
pub(crate) struct NewLink {
    /// `signed_in` or `anyone`.
    kind: LinkKind,
    /// A role id.
    role: String,
    /// RFC 3339; required for an anyone link.
    expires_at: Option<String>,
}

/// `POST /api/nodes/{id}/links {kind, role, expires_at?}`: makes a link to the node giving
/// `role`, and answers `201` with it and its address, which is never shown again. Needs
/// `manage_access` on the node; the role must be within the caller's own access there
/// (`403 beyond_your_access`) and may not manage access or the site (`400
/// link_role_too_high`; an anyone link only ever views). An anyone link needs anyone links
/// switched on (`400 anyone_links_off`) and an expiry (`400 expiry_required`) no further away
/// than the organisation allows (`400 expiry_too_long`); an expiry must be in the future (`400
/// expiry_in_past`). A signed-in link gives the node its own permissions if it inherited.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    JsonBody(new): JsonBody<NewLink>,
) -> Result<Response, ApiError> {
    let node = node_id(&id)?;
    let expires_at = match new.expires_at.as_deref() {
        Some(text) => {
            Some(OffsetDateTime::parse(text, &Rfc3339).map_err(|_| ApiError::InvalidRequest)?)
        }
        None => None,
    };
    // The API's guard has checked that `Host` names this machine.
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or(ApiError::Forbidden)?
        .to_owned();
    let actor = signed.session.user.id;
    // Breaking inheritance copies what the node inherited, and a new link has no members: no
    // open session's access changes, so nothing is rechecked.
    let (link, token) = state
        .db()
        .create_link(actor, node, new.kind, &new.role, expires_at)
        .await?;
    let url = link_url(&host, &token);
    Ok((
        StatusCode::CREATED,
        Json(NewLinkOut {
            link: link.into(),
            url,
        }),
    )
        .into_response())
}

/// The address of the link with `token` on the server reached as `host`.
fn link_url(host: &str, token: &LinkToken) -> String {
    format!("http://{host}/l/{}", token.as_url_value())
}

/// `DELETE /api/links/{id}`: revokes the link at once. Needs `manage_access` on its item. A
/// signed-in link's assignments (on its item and wherever a break below copied them) and
/// members are removed, and every open document below them is rechecked before the answer.
/// `204`, also for a link already revoked; `404` for an unknown link.
pub(crate) async fn revoke(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let link = parse_id(&id)?;
    let actor = signed.session.user.id;
    to_completion(async move {
        let touched = state.db().revoke_link(Some(actor), link, "manager").await?;
        for node in touched.unwrap_or_default() {
            recheck_below(&state, node).await;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
