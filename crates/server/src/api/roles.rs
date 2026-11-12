//! Roles: named sets of permissions. Anyone who manages access somewhere may list them, to
//! give them; only administrators define, change and delete them.
//!
//! A change to a role's permissions changes the access of everyone holding it, everywhere, so
//! once it is stored every open document is rechecked before the answer.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use records::{AccessError, Perms, RoleDef};
use serde::Deserialize;

use super::{ApiError, JsonBody, recheck_everyone, to_completion};
use crate::AppState;
use crate::auth::Signed;

/// `GET /api/roles`: every role (`{id, name, perms, builtin}`), the built-in ones first (Viewer,
/// Editor, Owner), then the others by name. For administrators and anyone who manages access
/// somewhere; `403` for anyone else.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<RoleDef>>, ApiError> {
    let db = state.db();
    let allowed = db
        .manages_access_anywhere(signed.session.user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    if !allowed {
        return Err(ApiError::Forbidden);
    }
    let roles = db.roles().await.map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(roles))
}

/// Permissions named in a request body, or `422 invalid_request` for a name that is not one.
fn perms_of(names: &[String]) -> Result<Perms, ApiError> {
    Perms::from_names(names).ok_or(ApiError::InvalidRequest)
}

/// The body of `POST /api/roles`.
#[derive(Deserialize)]
pub(crate) struct NewRole {
    name: String,
    perms: Vec<String>,
}

/// `POST /api/roles {name, perms}`: defines a role with these permissions (`view` is always
/// added). Administrators only. `422 invalid_name` for an empty or too long name, `409
/// name_taken` for one another role has, ignoring case. `201` with the role.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewRole>,
) -> Result<Response, ApiError> {
    let perms = perms_of(&new.perms)?;
    let role = state
        .db()
        .create_role(signed.session.user.id, &new.name, perms)
        .await?;
    Ok((StatusCode::CREATED, Json(role)).into_response())
}

/// A failure of a change to the role named in the path: an unknown role is `404`.
fn role_error(e: AccessError) -> ApiError {
    match e {
        AccessError::NoSuchRole => ApiError::NotFound,
        e => e.into(),
    }
}

/// The body of `PATCH /api/roles/{id}`.
#[derive(Deserialize)]
pub(crate) struct RoleChange {
    name: Option<String>,
    perms: Option<Vec<String>>,
}

/// `PATCH /api/roles/{id} {name?, perms?}`: renames a custom role, changes its permissions, or
/// both. Administrators only; `400 builtin_role` for a built-in role, `409 last_manager` if it
/// would lose `manage_access` where it gives a scope's only manager. Every open document is
/// rechecked before the answer, so that everyone holding the role has its new permissions in
/// their open sessions at once. Answers the role as it then is.
pub(crate) async fn change(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(change): JsonBody<RoleChange>,
) -> Result<Json<RoleDef>, ApiError> {
    let perms = change.perms.as_deref().map(perms_of).transpose()?;
    if change.name.is_none() && perms.is_none() {
        return Err(ApiError::InvalidRequest);
    }
    let actor = signed.session.user.id;
    let role = to_completion(async move {
        let role = state
            .db()
            .change_role(actor, &id, change.name.as_deref(), perms)
            .await
            .map_err(role_error)?;
        recheck_everyone(&state).await;
        Ok(role)
    })
    .await?;
    Ok(Json(role))
}

/// `DELETE /api/roles/{id}`: deletes a custom role. Administrators only; `400 builtin_role` for a
/// built-in role, `409 role_in_use` while any assignment gives it. `204`.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // A role in use cannot be deleted, so nobody's access changes and no room is told.
    state
        .db()
        .delete_role(signed.session.user.id, &id)
        .await
        .map_err(role_error)?;
    Ok(StatusCode::NO_CONTENT)
}
