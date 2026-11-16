//! Applying a look to many sites at once, for administrators: one look given in full, the look
//! of another site, the look a template carries, or a brand-centre theme and font package
//! (which change only the theme and fonts), applied to each site in turn, each with its own
//! commit and history entry (see [`crate::looks`]).

use axum::Json;
use axum::extract::State;
use collab_core::ids::NodeId;
use records::User;
use revisions::brand::{Actor, TemplateLook};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::brand::completed;
use super::{ApiError, JsonBody};
use crate::AppState;
use crate::auth::Signed;
use crate::brand::{BrandError, actor_kind};
use crate::looks::{self, LookInput};
use crate::templates::Unresolved;

/// The most sites one request applies a look to.
pub(crate) const MAX_SITES: usize = 200;

/// The largest body of `POST /api/admin/looks/apply`: a look as `PUT /api/sites/{id}/look`
/// takes it, and the identifiers of the sites.
pub(crate) const MAX_APPLY_BODY: usize = super::looks::MAX_LOOK_BODY + 64 * 1024;

/// Where the look applied comes from.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// A look given in full, as `PUT /api/sites/{id}/look` takes it.
    Look(Box<LookInput>),
    /// The look of this site, as `POST /api/sites/{id}/look/copy` copies it (without the
    /// source's own themes and font packages).
    FromSite(String),
    /// The look this template carries: a saved template's id (a built-in carries none).
    Template(String),
    /// A brand-centre theme and font package: only each site's theme and fonts change.
    Brand(BrandSource),
}

/// A theme and font package to apply, each named as a look names its source (`builtin:<id>`
/// or `org:<id>`); no `fonts` for the system fonts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrandSource {
    theme: String,
    #[serde(default)]
    fonts: Option<String>,
}

/// The body of `POST /api/admin/looks/apply`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyBody {
    sites: Vec<String>,
    source: Source,
}

/// `POST /api/admin/looks/apply {sites: [id], source: {look} | {from_site} | {template} |
/// {brand: {theme, fonts?}}}`:
/// applies one look to each of 1 to 200 sites, in the order given (an id given twice is applied
/// once). Administrators only (`403`), who are never limited by the brand policy. Each site is
/// changed as `PUT /api/sites/{id}/look`, a copy, or a site made from the template would change
/// it (a `brand` source changes only its theme and fonts, keeping its logo, thumbnail, tab
/// icon, header, navigation, menu and footer), with its own commit and its own `site.look_changed` entry marked `applied_by_admin`; the
/// same look again changes nothing. `422 invalid_request` for no sites, more than 200, or a
/// source site that is not an id; `422 invalid_template {reason: "no_look"}` for a template
/// that carries no look; `404` for an unknown template. `200` with `{results: [{site, ok,
/// error?, ...}]}`, one per site in order: a site refused gives its error as its own request
/// would (`archived` for an archived site, `not_found`, `invalid_look`, ...), and a template's
/// references that site could not fill as `unresolved`.
pub(crate) async fn apply(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<ApplyBody>,
) -> Result<Json<Value>, BrandError> {
    let actor = signed.session.user;
    if actor_kind(state.db(), &actor).await? != Actor::Admin {
        return Err(ApiError::Forbidden.into());
    }
    if body.sites.is_empty() || body.sites.len() > MAX_SITES {
        return Err(ApiError::InvalidRequest.into());
    }
    let mut sites: Vec<String> = Vec::new();
    for site in body.sites {
        if !sites.contains(&site) {
            sites.push(site);
        }
    }
    let applied = match body.source {
        Source::Look(input) => Applied::Look(input),
        Source::FromSite(from) => Applied::FromSite(parse(&from)?),
        Source::Brand(brand) => Applied::Brand(brand),
        Source::Template(id) => {
            let look = crate::templates::look_of(&state, &actor, &id)
                .await?
                .ok_or(ApiError::InvalidTemplate("no_look"))?;
            Applied::Template(id, Box::new(look))
        }
    };
    completed(async move {
        let mut results = Vec::new();
        for site in sites {
            let result = match parse(&site) {
                Ok(node) => apply_one(&state, &actor, node, &applied).await,
                Err(e) => Err(e),
            };
            results.push(match result {
                Ok(unresolved) if unresolved.is_empty() => json!({ "site": site, "ok": true }),
                Ok(unresolved) => json!({ "site": site, "ok": true, "unresolved": unresolved }),
                Err(e) => failed(&site, &e),
            });
        }
        Ok(Json(json!({ "results": results })))
    })
    .await
}

/// A source made ready to apply.
enum Applied {
    /// A look given in full.
    Look(Box<LookInput>),
    /// The look of this site.
    FromSite(NodeId),
    /// The look of the template with this id.
    Template(String, Box<TemplateLook>),
    /// A brand-centre theme and font package.
    Brand(BrandSource),
}

/// Applies `applied` to `site` for `actor`, an administrator, marking its history entry
/// `applied_by_admin`. Answers the references of a template's look that were left out.
async fn apply_one(
    state: &AppState,
    actor: &User,
    site: NodeId,
    applied: &Applied,
) -> Result<Vec<Unresolved>, BrandError> {
    let marked = || vec![("applied_by_admin", json!(true))];
    match applied {
        Applied::Look(input) => {
            looks::change_look(state, actor, site, (**input).clone(), marked()).await?;
            Ok(Vec::new())
        }
        Applied::FromSite(from) => {
            looks::copy_into(state, actor, site, *from, (false, false), marked()).await?;
            Ok(Vec::new())
        }
        Applied::Template(id, look) => {
            looks::apply_template_look(state, actor, site, id, look).await
        }
        Applied::Brand(brand) => {
            let chosen = (brand.theme.as_str(), brand.fonts.as_deref());
            looks::apply_brand(state, actor, site, chosen, marked()).await?;
            Ok(Vec::new())
        }
    }
}

/// A site's id as sent, or `invalid_request`.
fn parse(site: &str) -> Result<NodeId, BrandError> {
    Uuid::parse_str(site)
        .map(NodeId)
        .map_err(|_| ApiError::InvalidRequest.into())
}

/// The result of a site refused with `error`.
fn failed(site: &str, error: &BrandError) -> Value {
    let mut out = error.as_result();
    if let Some(map) = out.as_object_mut() {
        map.insert("site".into(), json!(site));
        map.insert("ok".into(), json!(false));
    }
    out
}
