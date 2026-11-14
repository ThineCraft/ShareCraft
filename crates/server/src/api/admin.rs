//! Managing people and sites (restoring and purging deleted sites). Administrators only.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{NodeId, UserId};
use records::{AdminSite, BIN_DAYS, NewUser};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::nodes::rfc3339;
use super::{ApiError, JsonBody, after_account_disabled, parse_id, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::tree;

/// Refuses everyone but administrators, before any other work. The records layer checks again.
fn require_admin(signed: &Signed) -> Result<(), ApiError> {
    if signed.session.user.is_admin {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// One row of `GET /api/admin/users`.
#[derive(Serialize)]
pub(crate) struct Person {
    id: String,
    email: String,
    name: String,
    is_admin: bool,
    disabled: bool,
}

/// `GET /api/admin/users`: every account.
pub(crate) async fn users(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<Person>>, ApiError> {
    require_admin(&signed)?;
    let users = state
        .db()
        .users()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(
        users
            .into_iter()
            .map(|user| Person {
                id: user.id.0.to_string(),
                email: user.email,
                name: user.name,
                is_admin: user.is_admin,
                disabled: user.disabled,
            })
            .collect(),
    ))
}

/// The body of `POST /api/admin/users`. No `Debug`: it holds a password.
#[derive(Deserialize)]
pub(crate) struct NewPerson {
    email: String,
    name: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
}

/// `POST /api/admin/users`: creates an account.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewPerson>,
) -> Result<Response, ApiError> {
    require_admin(&signed)?;
    let user = state
        .db()
        .create_user(
            Some(signed.session.user.id),
            NewUser {
                email: &new.email,
                name: &new.name,
                password: &new.password,
                is_admin: new.is_admin,
            },
        )
        .await?;
    let body = Json(serde_json::json!({ "id": user.id.0.to_string() }));
    Ok((StatusCode::CREATED, body).into_response())
}

/// `POST /api/admin/users/{id}/disable`: disables an account and ends its sessions.
pub(crate) async fn disable(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&signed)?;
    let user = UserId(parse_id(&id)?);
    let actor = signed.session.user.id;
    // On its own task, so that a disabled account is removed even if the request is cancelled.
    to_completion(async move {
        state.db().set_disabled(actor, user, true).await?;
        after_account_disabled(&state, user);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/admin/users/{id}/enable`: re-enables an account.
pub(crate) async fn enable(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&signed)?;
    let user = UserId(parse_id(&id)?);
    state
        .db()
        .set_disabled(signed.session.user.id, user, false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// One row of `GET /api/admin/sites`.
#[derive(Serialize)]
pub(crate) struct SiteRow {
    id: String,
    name: String,
    /// `active`, `archived` or `deleted` (a deleted site may also be archived: it comes back so).
    state: &'static str,
    /// `team` or `communication`.
    kind: &'static str,
    is_hub: bool,
    /// The hub it joined, `{id, name}`.
    hub: Option<Value>,
    /// Bytes held by every version of every item, live and in its bins.
    used: u64,
    /// Its quota in bytes; `null` for no limit.
    quota: Option<u64>,
    /// How many people own it.
    owners: i64,
    /// For a deleted site, `{at, by}`: when, and the name of whoever deleted it.
    deleted: Option<Value>,
    /// For a deleted site, when it will be purged: 93 days after it was deleted.
    purge_at: Option<String>,
    /// For a restored site whose name was taken meanwhile, the name it had.
    #[serde(skip_serializing_if = "Option::is_none")]
    renamed_from: Option<String>,
}

impl SiteRow {
    fn of(site: AdminSite) -> SiteRow {
        let state = match (site.deleted_at, site.archived) {
            (Some(_), _) => "deleted",
            (None, true) => "archived",
            (None, false) => "active",
        };
        SiteRow {
            id: site.id.0.to_string(),
            name: site.name,
            state,
            kind: site.kind.as_str(),
            is_hub: site.is_hub,
            hub: site
                .hub
                .map(|(id, name)| json!({ "id": id.0.to_string(), "name": name })),
            used: site.used,
            quota: site.quota,
            owners: site.owners,
            deleted: site
                .deleted_at
                .map(|at| json!({ "at": rfc3339(Some(at)), "by": site.deleted_by })),
            purge_at: rfc3339(
                site.deleted_at
                    .map(|at| at + time::Duration::days(BIN_DAYS)),
            ),
            renamed_from: None,
        }
    }
}

/// Every site that is not purged, as the administrators' Sites page lists it.
async fn site_rows(state: &AppState) -> Result<Vec<SiteRow>, ApiError> {
    Ok(state
        .db()
        .admin_sites()
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .map(SiteRow::of)
        .collect())
}

/// `GET /api/admin/sites`: every site, live, archived or deleted (not purged), by name, with its
/// `state`, `kind`, `is_hub`, `hub`, `used`, `quota`, number of `owners`, and for a deleted
/// site `deleted {at, by}` and `purge_at`. Administrators only.
pub(crate) async fn sites(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<SiteRow>>, ApiError> {
    require_admin(&signed)?;
    Ok(Json(site_rows(&state).await?))
}

/// `POST /api/admin/sites/{id}/restore`: restores the deleted site as it was (see
/// [`tree::restore_site`]), recorded as `site.restored`. Answers its row of
/// `GET /api/admin/sites`, with `renamed_from` when its name had been taken meanwhile.
/// Administrators only; `409 not_deleted` for a site that is not deleted; `404` for anything
/// but a site not purged.
pub(crate) async fn restore_site(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<SiteRow>, ApiError> {
    require_admin(&signed)?;
    let site = NodeId(parse_id(&id)?);
    let actor = signed.session.user;
    let applied = {
        let state = state.clone();
        to_completion(async move { Ok(tree::restore_site(&state, &actor, site).await?) }).await?
    };
    let mut row = site_rows(&state)
        .await?
        .into_iter()
        .find(|row| row.id == site.0.to_string())
        .ok_or(ApiError::NotFound)?;
    row.renamed_from = applied.renamed.map(|(_, from)| from);
    Ok(Json(row))
}

/// `DELETE /api/admin/sites/{id}`: purges the deleted site now (see [`tree::purge_site`]),
/// recorded as `site.purged`. Administrators only; `409 not_deleted` for a site that is not
/// deleted; `404` for anything but a site not purged. Answers `204`.
pub(crate) async fn purge_site(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&signed)?;
    let site = NodeId(parse_id(&id)?);
    let actor = signed.session.user;
    to_completion(async move { Ok(tree::purge_site(&state, Some(&actor), site).await?) }).await?;
    Ok(StatusCode::NO_CONTENT)
}
