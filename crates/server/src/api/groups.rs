//! Groups of people, which can be given roles like a person: listing the groups a person may
//! see, creating and deleting organisation groups, and managing members.
//!
//! A membership change can change what its person may do anywhere, so once it is stored every
//! open document they are connected to is rechecked before the answer.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{GroupId, NodeId, UserId};
use records::{AccessError, Db, Group, SiteGroup, User};
use serde::{Deserialize, Serialize};

use super::{ApiError, JsonBody, parse_id, recheck_person, to_completion};
use crate::AppState;
use crate::auth::Signed;

/// A site named for display: its id and its current name.
#[derive(Clone, Serialize)]
pub(crate) struct SiteRef {
    id: String,
    name: String,
}

impl SiteRef {
    pub(crate) fn new(id: NodeId, name: &str) -> SiteRef {
        SiteRef {
            id: id.0.to_string(),
            name: name.to_owned(),
        }
    }
}

/// A group as answered. A site group always carries its site (`{id, name}`) beside its name,
/// as its name can equal an organisation group's; an organisation group has `site: null`.
#[derive(Serialize)]
pub(crate) struct GroupOut {
    id: String,
    name: String,
    site: Option<SiteRef>,
    /// `owners`, `members` or `visitors` for a site group; `null` for an organisation group.
    purpose: Option<SiteGroup>,
    member_count: i64,
    /// Whether the caller may change the group's members.
    can_manage: bool,
}

impl GroupOut {
    /// `group` as answered; `can_manage` says whether the caller manages it.
    pub(crate) fn of(group: Group, can_manage: bool) -> GroupOut {
        let site = group
            .site
            .zip(group.site_name.as_deref())
            .map(|(id, name)| SiteRef::new(id, name));
        GroupOut {
            id: group.id.0.to_string(),
            name: group.name,
            site,
            purpose: group.purpose,
            member_count: group.member_count,
            can_manage,
        }
    }
}

/// Whether `user` manages `group`'s members: an enabled administrator manages every group,
/// someone with `manage_site` on a site its groups. The records layer decides again when a
/// member is added or removed.
async fn manages(db: &Db, user: &User, group: &Group) -> Result<bool, ApiError> {
    match db.manages_group(user.id, group.id).await {
        Ok(manages) => Ok(manages),
        // Deleted since it was read.
        Err(AccessError::NoSuchGroup) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// `groups` as answered to `user`, each saying whether they manage it.
pub(crate) async fn answered(
    db: &Db,
    user: &User,
    groups: Vec<Group>,
) -> Result<Vec<GroupOut>, ApiError> {
    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let can_manage = manages(db, user, &group).await?;
        out.push(GroupOut::of(group, can_manage));
    }
    Ok(out)
}

/// A group id from a path, or `422 invalid_request`.
fn group_id(text: &str) -> Result<GroupId, ApiError> {
    parse_id(text).map(GroupId)
}

/// `GET /api/groups`: the groups the caller may see, by name: none unless they are an
/// administrator or manage access somewhere; then the organisation groups and the groups of
/// the sites they can reach.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<GroupOut>>, ApiError> {
    let db = state.db();
    let user = signed.session.user;
    let groups = db
        .groups_visible_to(user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(answered(db, &user, groups).await?))
}

/// The body of `POST /api/groups`.
#[derive(Deserialize)]
pub(crate) struct NewGroup {
    name: String,
}

/// `POST /api/groups {name}`: creates an organisation group with no members. Administrators
/// only. `201` with the group.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewGroup>,
) -> Result<Response, ApiError> {
    let group = state
        .db()
        .create_group(signed.session.user.id, &new.name)
        .await?;
    let out = GroupOut::of(group, true);
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

/// A member as answered.
#[derive(Serialize)]
pub(crate) struct MemberOut {
    id: String,
    email: String,
    name: String,
}

/// `GET /api/groups/{id}`.
#[derive(Serialize)]
pub(crate) struct GroupDetail {
    #[serde(flatten)]
    group: GroupOut,
    /// The members, by name; empty unless the caller manages the group.
    members: Vec<MemberOut>,
}

/// `GET /api/groups/{id}`: the group and, for someone who manages it, its members. `404`
/// unless the caller may see the group.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<GroupDetail>, ApiError> {
    let id = group_id(&id)?;
    let db = state.db();
    let user = signed.session.user;
    let (group, members) = db
        .group(user.id, id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let can_manage = manages(db, &user, &group).await?;
    Ok(Json(GroupDetail {
        group: GroupOut::of(group, can_manage),
        members: members
            .into_iter()
            .map(|member| MemberOut {
                id: member.id.0.to_string(),
                email: member.email,
                name: member.name,
            })
            .collect(),
    }))
}

/// `DELETE /api/groups/{id}`: deletes an organisation group and its memberships.
/// Administrators only; `400 site_group` for a site's group, `409 group_in_use` while it is
/// assigned anywhere. `204`.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = group_id(&id)?;
    // A group in use cannot be deleted, so nobody's access changes and no room is told.
    state.db().delete_group(signed.session.user.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/groups/{id}/members`.
#[derive(Deserialize)]
pub(crate) struct NewMember {
    email: String,
}

/// `POST /api/groups/{id}/members {email}`: adds the person with this email to the group (see
/// [`add_member`]). The email is looked up only for someone who manages the group, so that
/// nobody else can learn which emails have accounts. `404 no_such_user` for an unknown email.
pub(crate) async fn add_by_email(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(new): JsonBody<NewMember>,
) -> Result<StatusCode, ApiError> {
    let id = group_id(&id)?;
    let db = state.db();
    let actor = signed.session.user;
    if !db.manages_group(actor.id, id).await? {
        return Err(ApiError::Forbidden);
    }
    let user = db
        .user_by_email(&new.email)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NoSuchUser)?;
    change_membership(state, actor.id, id, user.id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/groups/{id}/members/{user_id}`: makes the person a member of the group. An
/// administrator may change any group, someone with `manage_site` on a site its groups; anyone
/// but an administrator may only add a person to a group whose role gives, wherever it is
/// assigned, no permission they lack there themselves (`403 beyond_your_access`). Every open
/// document the person is connected to is rechecked before the answer. `204`, also when they
/// already were a member.
pub(crate) async fn add_member(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let group = group_id(&id)?;
    let user = UserId(parse_id(&user_id)?);
    change_membership(state, signed.session.user.id, group, user, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/groups/{id}/members/{user_id}`: ends the person's membership of the group, as
/// [`add_member`] decides who may. Every open document the person is connected to is
/// rechecked before the answer: they keep only what their other assignments give. `204`, also
/// when they were not a member.
pub(crate) async fn remove_member(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let group = group_id(&id)?;
    let user = UserId(parse_id(&user_id)?);
    change_membership(state, signed.session.user.id, group, user, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stores `actor`'s addition (`add`) or removal of `user` in `group` and rechecks `user` in
/// every open document, finishing both even if the request is cancelled.
async fn change_membership(
    state: AppState,
    actor: UserId,
    group: GroupId,
    user: UserId,
    add: bool,
) -> Result<(), ApiError> {
    to_completion(async move {
        let db = state.db();
        let changed = if add {
            db.add_member(actor, group, user).await?
        } else {
            db.remove_member(actor, group, user).await?
        };
        if changed {
            recheck_person(&state, user).await;
        }
        Ok(())
    })
    .await
}
