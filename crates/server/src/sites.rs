//! Sites' own settings: changing them (one Git commit with the site's renamed tree record when
//! the name changes too, then mirrored into PostgreSQL, taken back if refused), archiving and
//! reactivating a site, ending everything under way in a site, hubs (registering and
//! unregistering one, joining and leaving one), a site's own and the global navigation, and the
//! start-up reconcile of the site records, saved templates and global navigation with Git.
//!
//! A site created before site records has no record in Git: it reads as a team site with no
//! description and the organisation's default language and time zone, and its record is
//! written at its first change.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use collab_core::ids::{NodeId, UserId};
use protocol::RemovedReason;
use records::{
    AccessError, Db, DbError, NavScope, NewEvent, NodeKind, Perms, RegionalDefaults, SiteChange,
    SiteKind, SiteSettings, SiteState, SitesMirror, TemplateRow, User, validate_description,
    validate_language, validate_node_name, validate_time_zone,
};
use revisions::{
    CheckpointError, GitError, HubRecord, NavItem, OrgNavigation, SITE_SCHEMA, SiteRecord, Stamp,
    TemplateRecord,
};
use serde_json::{Map, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::AppState;
use crate::api::uploads::abandon_session;
use crate::checkpoint::Checkpointer;
use crate::room::AccessChange;
use crate::tree::{
    TreeError, Undo, author_of, ensure_in_step, git_failure, git_record, now_text, repair_ids,
    require, take_back_all, to_row, visible, write_records, write_sites,
};

/// What a person asks to change of a site: each field given is set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SiteEdit {
    /// The site's name.
    pub name: Option<String>,
    /// What the site is for; 0 to 500 characters.
    pub description: Option<String>,
    /// A BCP 47 language tag.
    pub language: Option<String>,
    /// An IANA time zone name.
    pub time_zone: Option<String>,
}

/// What a reconcile of the site records, templates and global navigation did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SitesReport {
    /// Sites in the tree.
    pub sites: usize,
    /// Libraries in the tree.
    pub libraries: usize,
    /// Saved templates in Git.
    pub templates: usize,
    /// Rows written because they differed from Git (the global navigation counts as one).
    pub updated: usize,
    /// Records that could not be written, and why.
    pub failed: Vec<(Uuid, String)>,
}

impl fmt::Display for SitesReport {
    /// One line with the counts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} sites, {} libraries, {} templates, {} updated, {} failed",
            self.sites,
            self.libraries,
            self.templates,
            self.updated,
            self.failed.len()
        )
    }
}

/// The record of a new site: a team site with no description and the organisation's defaults.
pub(crate) fn new_site_record(id: NodeId, defaults: &RegionalDefaults) -> SiteRecord {
    SiteRecord {
        schema: SITE_SCHEMA,
        id: id.0,
        kind: SiteKind::Team.as_str().to_owned(),
        description: String::new(),
        language: defaults.language.clone(),
        time_zone: defaults.time_zone.clone(),
        template: None,
        archived: None,
        hub: None,
        joined_hub: None,
        navigation: None,
        look: None,
        themes: Vec::new(),
        font_packages: Vec::new(),
    }
}

/// A site record as PostgreSQL holds it.
pub(crate) fn settings_of(record: &SiteRecord) -> Result<SiteSettings, String> {
    let kind = SiteKind::parse(&record.kind)
        .ok_or_else(|| format!("site {} has an unknown kind {:?}", record.id, record.kind))?;
    let archived_at = record
        .archived
        .as_ref()
        .map(|stamp| {
            OffsetDateTime::parse(&stamp.at, &Rfc3339).map_err(|e| {
                format!(
                    "site {} has an unreadable time {:?}: {e}",
                    record.id, stamp.at
                )
            })
        })
        .transpose()?;
    Ok(SiteSettings {
        id: NodeId(record.id),
        kind,
        description: record.description.clone(),
        language: record.language.clone(),
        time_zone: record.time_zone.clone(),
        template: record.template.clone(),
        archived_at,
        archived_by: record
            .archived
            .as_ref()
            .and_then(|stamp| Uuid::parse_str(&stamp.by.id).ok().map(UserId)),
        is_hub: record.hub.is_some(),
        hub_navigation: record.hub.as_ref().map(|hub| hub.navigation.clone()),
        joined_hub: record.joined_hub.map(NodeId),
        navigation: record.navigation.clone(),
        look: record.look.clone(),
        themes: record.themes.clone(),
        font_packages: record.font_packages.clone(),
        purging: false,
    })
}

/// A saved template as PostgreSQL holds it.
pub(crate) fn template_row(record: &TemplateRecord) -> Result<TemplateRow, String> {
    let time = |text: &str| {
        OffsetDateTime::parse(text, &Rfc3339).map_err(|e| {
            format!(
                "template {} has an unreadable time {text:?}: {e}",
                record.id
            )
        })
    };
    Ok(TemplateRow {
        id: record.id,
        name: record.name.clone(),
        description: record.description.clone(),
        body: serde_json::to_value(&record.body).map_err(|e| e.to_string())?,
        created_by: Uuid::parse_str(&record.created.by.id).ok().map(UserId),
        created_at: time(&record.created.at)?,
        deleted_at: record
            .deleted
            .as_ref()
            .map(|stamp| time(&stamp.at))
            .transpose()?,
    })
}

/// The record of `site` in Git, if it has one.
pub(crate) async fn read_site_record(
    store: &Arc<dyn Checkpointer>,
    site: NodeId,
) -> Result<Option<SiteRecord>, CheckpointError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || store.read_site_record(site.0))
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))?
}

/// Changes `site`'s name, description, language or time zone for `actor`, who needs
/// `manage_site` on it. Under the tree lock, the change is written to Git as one commit (the
/// site record, with the site's tree record when the name changes), then mirrored into
/// PostgreSQL with `site.changed` (the fields changed, each `{from, to}`), judged again there.
/// A refused change is taken back by a second commit. Values equal to the current ones change
/// nothing. A site with no record in Git gets one. Answers the settings as PostgreSQL then
/// holds them.
///
/// # Errors
/// `NotFound` when the site is missing, deleted or not visible to `actor`; `Forbidden` without
/// `manage_site`; `InvalidName`, `InvalidSetting` (a description longer than 500 characters),
/// `InvalidLanguage` or `InvalidTimeZone` for a value; `NameTaken` for a name another site
/// has; `OutOfStep` while the tree needs repair; otherwise the Git or database error.
pub async fn change(
    state: &AppState,
    actor: &User,
    site: NodeId,
    edit: SiteEdit,
) -> Result<SiteSettings, AccessError> {
    let name = edit
        .name
        .as_deref()
        .map(validate_node_name)
        .transpose()?
        .map(str::to_owned);
    if let Some(text) = &edit.description {
        validate_description(text)?;
    }
    if let Some(tag) = &edit.language {
        validate_language(tag)?;
    }
    if let Some(zone) = &edit.time_zone {
        validate_time_zone(zone)?;
    }

    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let (node, access) = visible(db, actor, site).await?;
    if node.kind != NodeKind::Site || node.deleted {
        return Err(AccessError::NotFound);
    }
    require(&access, Perms::MANAGE_SITE)?;
    let store = state.checkpointer();
    let stored = read_site_record(store, site).await.map_err(git_failure)?;
    let before = match &stored {
        Some(record) => record.clone(),
        None => new_site_record(site, &db.regional_defaults().await?),
    };
    let tree_before = git_record(state, site).await?;

    let mut after = before.clone();
    let mut fields = Map::new();
    let mut set = |field: &str, now: &mut String, wanted: Option<String>| {
        if let Some(wanted) = wanted
            && *now != wanted
        {
            fields.insert(field.to_owned(), json!({ "from": now, "to": wanted }));
            *now = wanted;
        }
    };
    let mut tree_after = tree_before.clone();
    set("name", &mut tree_after.name, name);
    set("description", &mut after.description, edit.description);
    set("language", &mut after.language, edit.language);
    set("time_zone", &mut after.time_zone, edit.time_zone);
    if fields.is_empty() {
        return db.site_settings(site).await?.ok_or(AccessError::NotFound);
    }
    let renamed = tree_after.name != tree_before.name;
    if renamed && !db.site_name_free(&tree_after.name, Some(site)).await? {
        return Err(AccessError::NameTaken);
    }

    let author = author_of(actor);
    let message = format!("Change site {}", tree_before.name);
    let tree = if renamed {
        vec![tree_after.clone()]
    } else {
        Vec::new()
    };
    write_sites(
        store,
        author.clone(),
        message.clone(),
        tree.clone(),
        vec![after.clone()],
        Vec::new(),
    )
    .await
    .map_err(git_failure)?;
    let undo = Undo {
        records: if renamed {
            vec![tree_before]
        } else {
            Vec::new()
        },
        sites: stored.clone().into_iter().collect(),
        removed_sites: if stored.is_none() {
            vec![site.0]
        } else {
            Vec::new()
        },
        ..Undo::default()
    };

    let settings = settings_of(&after).map_err(DbError::new)?;
    let row = if renamed {
        Some(to_row(&tree_after).map_err(DbError::new)?)
    } else {
        None
    };
    let applied = db
        .apply_site_change(SiteChange {
            settings: &settings,
            node: row.as_ref(),
            actor: Some(actor.id),
            needs_admin: false,
            needs: Perms::MANAGE_SITE,
            events: vec![NewEvent {
                kind: "site.changed",
                actor: Some(actor.id),
                subject: None,
                document: None,
                node: Some(site),
                detail: json!({ "name": tree_after.name, "fields": fields }),
            }],
        })
        .await;
    match applied {
        Ok(()) => db.site_settings(site).await?.ok_or(AccessError::NotFound),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: change to site {} is in Git but not in the database: {error}",
                site.0
            );
            let ids: Vec<Uuid> = tree.iter().map(|r| r.id).collect();
            if !(repair_ids(state, &ids).await && repair_sites(state, &[site]).await) {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: change to site {} could not be judged, so it is taken \
                     back: {error}",
                    site.0
                );
            }
            if !take_back_all(state, author, &message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// Archives `site` for `actor`, an enabled administrator: one Git commit writing the site's
/// record with `archived` stamped now by `actor`, mirrored into PostgreSQL with `site.archived`.
/// From then on every permission on the site's nodes is narrowed to `view`, for everyone, and
/// every write is refused with `Archived`. Then everything under way in the site is ended (see
/// [`end_site_activity`], with `archived`). Archiving an archived site changes nothing and
/// writes no commit. Answers the settings as PostgreSQL then holds them.
///
/// # Errors
/// `Forbidden` unless `actor` is an enabled administrator; `NotFound` for anything but a live
/// site; `OutOfStep` while the tree needs repair; otherwise the Git or database error.
pub async fn archive(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<SiteSettings, AccessError> {
    let settings = set_archived(state, actor, site, true).await?;
    end_site_activity(state, site, RemovedReason::Archived).await;
    Ok(settings)
}

/// Reactivates the archived `site` for `actor`, an enabled administrator: one Git commit
/// writing the site's record with no `archived` stamp, mirrored with `site.reactivated`; its
/// permissions are as they were before it was archived. Reactivating a site that is not
/// archived changes nothing and writes no commit. Answers the settings as PostgreSQL then
/// holds them.
///
/// # Errors
/// As [`archive`].
pub async fn reactivate(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<SiteSettings, AccessError> {
    set_archived(state, actor, site, false).await
}

/// Archives (`archived`) or reactivates `site` for `actor` (see [`archive`] and
/// [`reactivate`]), under the tree lock.
async fn set_archived(
    state: &AppState,
    actor: &User,
    site: NodeId,
    archived: bool,
) -> Result<SiteSettings, AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let current = db.user(actor.id).await?;
    if !current.is_some_and(|u| u.is_admin && !u.disabled) {
        return Err(AccessError::Forbidden);
    }
    live_site(db, site).await?;
    let node = db.node(site).await?.ok_or(AccessError::NotFound)?;
    let store = state.checkpointer();
    let stored = read_site_record(store, site).await.map_err(git_failure)?;
    let before = match &stored {
        Some(record) => record.clone(),
        None => new_site_record(site, &db.regional_defaults().await?),
    };
    if before.archived.is_some() == archived {
        return db.site_settings(site).await?.ok_or(AccessError::NotFound);
    }
    let author = author_of(actor);
    let mut after = before.clone();
    after.archived = archived.then(|| Stamp {
        at: now_text(),
        by: author.clone(),
    });
    let (verb, kind) = if archived {
        ("Archive", "site.archived")
    } else {
        ("Reactivate", "site.reactivated")
    };
    let message = format!("{verb} site {}", node.name);
    write_sites(
        store,
        author.clone(),
        message.clone(),
        Vec::new(),
        vec![after.clone()],
        Vec::new(),
    )
    .await
    .map_err(git_failure)?;
    let undo = Undo {
        sites: stored.clone().into_iter().collect(),
        removed_sites: if stored.is_none() {
            vec![site.0]
        } else {
            Vec::new()
        },
        ..Undo::default()
    };
    let settings = settings_of(&after).map_err(DbError::new)?;
    let applied = db
        .apply_site_change(SiteChange {
            settings: &settings,
            node: None,
            actor: Some(actor.id),
            needs_admin: true,
            needs: Perms::NONE,
            events: vec![NewEvent {
                kind,
                actor: Some(actor.id),
                subject: None,
                document: None,
                node: Some(site),
                detail: json!({ "name": node.name }),
            }],
        })
        .await;
    match applied {
        Ok(()) => db.site_settings(site).await?.ok_or(AccessError::NotFound),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: {kind} of site {} is in Git but not in the database: {error}",
                site.0
            );
            if !repair_sites(state, &[site]).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: {kind} of site {} could not be judged, so it is taken \
                     back: {error}",
                    site.0
                );
            }
            if !take_back_all(state, author, &message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// `Ok` for a live site; `SiteDeleted` for a deleted one, `NotFound` for anything else (a
/// purged site included).
async fn live_site(db: &Db, site: NodeId) -> Result<(), AccessError> {
    match db.site_state(site).await? {
        Some(SiteState::Live) => Ok(()),
        Some(SiteState::Deleted) => Err(AccessError::SiteDeleted),
        Some(SiteState::Purged) | None => Err(AccessError::NotFound),
    }
}

/// Deletes the site `site` for `actor`, an enabled administrator (an archived site included):
/// one Git commit marking its site node deleted, mirrored with `site.deleted` (see
/// [`crate::tree::apply`]); then everything under way in it is ended (see
/// [`end_site_activity`], with `deleted`). From then on everything in it answers as missing to
/// everyone, until an administrator restores it ([`crate::tree::restore_site`]) or it is
/// purged ([`crate::tree::purge_site`]), 93 days after the deletion at the latest.
///
/// # Errors
/// `Forbidden` unless `actor` is an enabled administrator; `SiteDeleted` for a deleted site;
/// `NotFound` for anything but a site; `OutOfStep` while the tree needs repair; otherwise the
/// Git or database error.
pub async fn delete(state: &AppState, actor: &User, site: NodeId) -> Result<(), AccessError> {
    let db = state.db();
    if !db
        .user(actor.id)
        .await?
        .is_some_and(|u| u.is_admin && !u.disabled)
    {
        return Err(AccessError::Forbidden);
    }
    live_site(db, site).await?;
    crate::tree::apply(state, actor, crate::tree::TreeOp::Delete { id: site }).await?;
    end_site_activity(state, site, RemovedReason::Deleted).await;
    Ok(())
}

/// Every site whose record in Git names `hub` as the hub it joined, each as it is (first) and
/// with `joined_hub` cleared (second): what a commit that releases the hub's members writes,
/// with their rows cleared in the same transaction (`records`' `release_hub_rows`). Purging a
/// hub releases them, and so does unregistering one ([`unregister_hub`]). A store that keeps no site records has none.
///
/// # Errors
/// The Git error, as a database error.
pub(crate) async fn released_members(
    state: &AppState,
    hub: NodeId,
) -> Result<Vec<(SiteRecord, SiteRecord)>, AccessError> {
    let store = Arc::clone(state.checkpointer());
    let records = tokio::task::spawn_blocking(move || store.read_sites())
        .await
        .map_err(|e| git_failure(CheckpointError::Git(GitError::new(e))))?;
    let records = match records {
        Ok(records) => records,
        Err(CheckpointError::Unsupported) => return Ok(Vec::new()),
        Err(e) => return Err(git_failure(e)),
    };
    Ok(records
        .into_iter()
        .filter(|record| record.joined_hub == Some(hub.0))
        .map(|before| {
            let mut after = before.clone();
            after.joined_hub = None;
            (before, after)
        })
        .collect())
}

/// Ends everything under way in `site`: every editing session (text and file) of a document
/// in the site is ended, each person in it removed with `Removed { reason }`, and every upload
/// session into the site is abandoned, its part file removed and its reservation of the site's
/// quota released. Archiving calls it with `archived`, deleting a site with `deleted`. Failures
/// are logged and do not stop the rest: a person left in a session keeps only what the access
/// decision now gives them, and an upload left open is refused at its next chunk or finish.
pub(crate) async fn end_site_activity(state: &AppState, site: NodeId, reason: RemovedReason) {
    let db = state.db();
    for doc in state.rooms().open_documents() {
        match db.node(doc.into()).await {
            Ok(Some(node)) if node.site == site => {
                // Removal is final, so it needs no ordering with other access changes.
                state.rooms().send_to(doc, AccessChange::Ended { reason });
            }
            Ok(_) => {}
            Err(e) => eprintln!("sharecraft-server: database failure: {e}"),
        }
    }
    let uploads = match db.uploads_in_site(site).await {
        Ok(uploads) => uploads,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return;
        }
    };
    for id in uploads {
        if let Err(e) = abandon_session(state, id).await {
            eprintln!(
                "sharecraft-server: could not abandon an upload into site {}: {}",
                site.0,
                e.code()
            );
        }
    }
}

/// Reconciles the rows of `sites` from Git once. Returns whether every one is now in step.
pub(crate) async fn repair_sites(state: &AppState, sites: &[NodeId]) -> bool {
    if sites.is_empty() {
        return true;
    }
    let db = state.db();
    let defaults = match db.regional_defaults().await {
        Ok(defaults) => defaults,
        Err(e) => {
            eprintln!("sharecraft-server: could not repair sites: {e}");
            return false;
        }
    };
    let current: HashMap<NodeId, SiteSettings> = match db.all_site_settings().await {
        Ok(rows) => rows.into_iter().map(|row| (row.id, row)).collect(),
        Err(e) => {
            eprintln!("sharecraft-server: could not repair sites: {e}");
            return false;
        }
    };
    for site in sites {
        let pins = match db.brand_pins(&site.0.to_string()).await {
            Ok(pins) => pins,
            Err(e) => {
                eprintln!("sharecraft-server: could not repair sites: {e}");
                return false;
            }
        };
        let record = match read_site_record(state.checkpointer(), *site).await {
            Ok(record) => record,
            Err(e) => {
                eprintln!(
                    "sharecraft-server: could not read site {} from Git: {e}",
                    site.0
                );
                return false;
            }
        };
        let record = record.unwrap_or_else(|| new_site_record(*site, &defaults));
        if let Err(e) = repair_site(db, &record, current.get(site), &pins).await {
            eprintln!("sharecraft-server: could not repair site {}: {e}", site.0);
            return false;
        }
    }
    true
}

/// After the organisation's regional defaults change, brings the rows of the sites with no
/// record in Git to the new defaults, as such a site is read as the organisation's current
/// defaults: only their language and time zone change, and no event is recorded for them (the
/// settings change is the event). The caller holds the tree lock, so no site record is written
/// meanwhile. A row that differs from a record-less site in anything else, or of a site whose
/// purge has started, is left for the reconcile. Returns whether every row was written; on a
/// failure the caller marks the tree out of step, so the reconcile finishes the job.
pub(crate) async fn follow_defaults(state: &AppState) -> bool {
    let store = Arc::clone(state.checkpointer());
    let records = match tokio::task::spawn_blocking(move || store.read_sites()).await {
        Ok(Ok(records)) => records,
        Ok(Err(CheckpointError::Unsupported)) => return true,
        Ok(Err(e)) => {
            eprintln!("sharecraft-server: could not read the site records from Git: {e}");
            return false;
        }
        Err(e) => {
            eprintln!("sharecraft-server: could not read the site records from Git: {e}");
            return false;
        }
    };
    let recorded: HashSet<Uuid> = records.iter().map(|record| record.id).collect();
    let db = state.db();
    let (defaults, nodes, rows) = match tokio::try_join!(
        db.regional_defaults(),
        db.node_rows(),
        db.all_site_settings()
    ) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("sharecraft-server: could not bring sites to the new defaults: {e}");
            return false;
        }
    };
    let live: HashSet<NodeId> = nodes
        .into_iter()
        .filter(|row| row.kind == NodeKind::Site && row.purged_at.is_none())
        .map(|row| row.id)
        .collect();
    let mut all_written = true;
    for current in rows {
        if recorded.contains(&current.id.0) || !live.contains(&current.id) || current.purging {
            continue;
        }
        let Ok(wanted) = settings_of(&new_site_record(current.id, &defaults)) else {
            continue;
        };
        let mut followed = current.clone();
        followed.language.clone_from(&wanted.language);
        followed.time_zone.clone_from(&wanted.time_zone);
        if current.same_record(&wanted) || !followed.same_record(&wanted) {
            continue;
        }
        let applied = db
            .apply_site_change(SiteChange {
                settings: &followed,
                node: None,
                actor: None,
                needs_admin: false,
                needs: Perms::NONE,
                events: Vec::new(),
            })
            .await;
        if let Err(e) = applied {
            eprintln!(
                "sharecraft-server: could not bring site {} to the new defaults: {e}",
                current.id.0
            );
            all_written = false;
        }
    }
    all_written
}

/// The pins a site asks for: each content its look and its own font packages name, with its type,
/// by hash.
fn wanted_pins(settings: &SiteSettings) -> Vec<(String, String)> {
    let mut pins: Vec<(String, String)> = settings
        .pins()
        .into_iter()
        .map(|pin| (pin.sha256.to_owned(), pin.media_type.to_owned()))
        .collect();
    pins.sort();
    pins
}

/// Writes `record` into PostgreSQL, with the pins of its look's images, if `current` or the
/// site's pins `pins` (each `(sha256, media_type)`, by hash) differ from it, with
/// `site.changed` marked as a repair. Returns whether it wrote.
async fn repair_site(
    db: &Db,
    record: &SiteRecord,
    current: Option<&SiteSettings>,
    pins: &[(String, String)],
) -> Result<bool, String> {
    let settings = settings_of(record)?;
    if current.is_some_and(|now| now.same_record(&settings)) && wanted_pins(&settings) == pins {
        return Ok(false);
    }
    db.apply_site_change(SiteChange {
        settings: &settings,
        node: None,
        actor: None,
        needs_admin: false,
        needs: Perms::NONE,
        events: vec![NewEvent {
            kind: "site.changed",
            actor: None,
            subject: None,
            document: None,
            node: Some(settings.id),
            detail: json!({ "repaired": true }),
        }],
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Brings PostgreSQL's mirror of the site records, saved templates, library records and global
/// navigation up to date with Git: every site of the tree (not purged) whose row (look and own
/// themes included) or the pins of its look's images are missing or differ is written from its
/// record, or for a site with none from the organisation's current defaults, with
/// `site.changed` marked `"repaired": true` (hubs before the sites that joined them); the pins
/// of a site no longer in the tree are dropped (each owner counts as one row); likewise each
/// saved template (`template.saved`), then each library, after the sites (see
/// [`crate::libraries::reconcile`], `library.changed`), the global navigation
/// (`navigation.changed`) and the brand centre with the images it pins (see
/// [`crate::brand::reconcile`]; the brand centre counts as one row). A record that cannot be
/// written, or whose site is not in the tree, does not stop the others; it is listed in the
/// report.
///
/// # Errors
/// The Git error if the records cannot be read (`Unsupported` for a store without them), or
/// the database error if the rows cannot be listed.
pub async fn reconcile(store: Arc<dyn Checkpointer>, db: &Db) -> Result<SitesReport, TreeError> {
    let (records, templates, navigation) = {
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || {
            Ok::<_, CheckpointError>((
                store.read_sites()?,
                store.read_templates()?,
                store.read_org_navigation()?,
            ))
        })
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))??
    };
    let defaults = db.regional_defaults().await?;
    let sites: HashSet<NodeId> = db
        .node_rows()
        .await?
        .into_iter()
        .filter(|row| row.kind == NodeKind::Site && row.purged_at.is_none())
        .map(|row| row.id)
        .collect();
    let current: HashMap<NodeId, SiteSettings> = db
        .all_site_settings()
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    let mut report = SitesReport {
        sites: sites.len(),
        templates: templates.len(),
        ..SitesReport::default()
    };
    let mut pins: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (owner, sha, media_type) in db.all_brand_pins().await? {
        pins.entry(owner).or_default().push((sha, media_type));
    }
    // Pins of a site no longer in the tree (purged, or never mirrored) are dropped; the
    // sweeper erases what nothing names any more.
    for owner in pins.keys() {
        let gone = Uuid::parse_str(owner).is_ok_and(|id| !sites.contains(&NodeId(id)));
        if gone {
            db.drop_brand_pins(owner).await?;
            report.updated = report.updated.saturating_add(1);
        }
    }

    let mut by_site: HashMap<NodeId, SiteRecord> = HashMap::new();
    for record in records {
        if sites.contains(&NodeId(record.id)) {
            by_site.insert(NodeId(record.id), record);
        } else if current.contains_key(&NodeId(record.id)) {
            // A purged site's record stays as it is; its row is not rewritten.
        } else {
            report
                .failed
                .push((record.id, "the site is not in the tree".to_owned()));
        }
    }
    let mut wanted: Vec<SiteRecord> = sites
        .iter()
        .map(|site| {
            by_site
                .remove(site)
                .unwrap_or_else(|| new_site_record(*site, &defaults))
        })
        .collect();
    // Hubs first, so a joined hub's row exists before the rows that name it.
    wanted.sort_by_key(|record| (record.joined_hub.is_some(), record.id));
    for record in &wanted {
        let own = pins
            .get(&record.id.to_string())
            .map_or(&[][..], Vec::as_slice);
        match repair_site(db, record, current.get(&NodeId(record.id)), own).await {
            Ok(true) => report.updated = report.updated.saturating_add(1),
            Ok(false) => {}
            Err(error) => report.failed.push((record.id, error)),
        }
    }

    let rows: HashMap<Uuid, TemplateRow> = db
        .template_rows()
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    for record in &templates {
        let row = match template_row(record) {
            Ok(row) => row,
            Err(error) => {
                report.failed.push((record.id, error));
                continue;
            }
        };
        if rows.get(&row.id).is_some_and(|now| now.same_record(&row)) {
            continue;
        }
        let event = NewEvent {
            kind: "template.saved",
            actor: None,
            subject: None,
            document: None,
            node: None,
            detail: json!({ "repaired": true, "template": row.id, "name": row.name }),
        };
        match db.apply_template(&row, Some(event)).await {
            Ok(()) => report.updated = report.updated.saturating_add(1),
            Err(e) => report.failed.push((record.id, e.to_string())),
        }
    }

    // After the sites, as a library's site comes before it.
    let libraries = crate::libraries::reconcile(Arc::clone(&store), db).await?;
    report.libraries = libraries.libraries;
    report.updated = report.updated.saturating_add(libraries.updated);
    report.failed.extend(libraries.failed);

    let items = navigation
        .map(|n: OrgNavigation| n.navigation)
        .unwrap_or_default();
    if db.global_navigation().await? != items {
        let event = NewEvent {
            kind: "navigation.changed",
            actor: None,
            subject: None,
            document: None,
            node: None,
            detail: json!({ "which": "global", "repaired": true }),
        };
        db.apply_global_navigation(&items, Some(event)).await?;
        report.updated = report.updated.saturating_add(1);
    }

    // After the sites, as a site's look pins brand images as the brand centre does.
    let brand = crate::brand::reconcile(Arc::clone(&store), db).await?;
    report.updated = report.updated.saturating_add(brand);
    Ok(report)
}

/// The record of `site` in Git (`None` if it has none), and the record a change starts from:
/// that one, or for a site with none a new record with the organisation's defaults.
pub(crate) async fn record_and_start(
    state: &AppState,
    site: NodeId,
) -> Result<(Option<SiteRecord>, SiteRecord), AccessError> {
    let stored = read_site_record(state.checkpointer(), site)
        .await
        .map_err(git_failure)?;
    let start = match &stored {
        Some(record) => record.clone(),
        None => new_site_record(site, &state.db().regional_defaults().await?),
    };
    Ok((stored, start))
}

/// The live site `site` as `actor` sees it, with their access: `NotFound` unless it is a site
/// they can see that is not deleted.
pub(crate) async fn seen_site(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<(records::Node, records::Access), AccessError> {
    let (node, access) = visible(state.db(), actor, site).await?;
    if node.kind != NodeKind::Site || node.deleted {
        return Err(AccessError::NotFound);
    }
    Ok((node, access))
}

/// `Ok` if `actor` is an enabled administrator; otherwise the refusal of an administrators'
/// route on `site`: `Archived` when they can see the site and it is archived, `Forbidden`
/// otherwise.
async fn admin_on(db: &Db, actor: &User, site: NodeId) -> Result<(), AccessError> {
    if db
        .user(actor.id)
        .await?
        .is_some_and(|u| u.is_admin && !u.disabled)
    {
        return Ok(());
    }
    Err(match db.effective_access(actor.id, site).await? {
        Some(access) => access.refusal(),
        None => AccessError::Forbidden,
    })
}

/// A change of site records: each changed site's record as Git held it (`None` if it had
/// none) and as it is to be, with what its mirror must judge and record.
pub(crate) struct SitesCommit {
    /// The commit message.
    pub(crate) message: String,
    /// Each site's record before (`None` if Git had none) and after.
    pub(crate) changed: Vec<(Option<SiteRecord>, SiteRecord)>,
    /// Whether the actor must be an enabled administrator.
    pub(crate) needs_admin: bool,
    /// Nodes the actor must hold the given permissions on.
    pub(crate) needs: Vec<(NodeId, Perms)>,
    /// Sites whose rows are locked too, unchanged.
    pub(crate) also_lock: Vec<NodeId>,
    /// A hub whose members leave it.
    pub(crate) release_hub: Option<NodeId>,
    /// The events recorded with the change.
    pub(crate) events: Vec<NewEvent>,
}

/// Writes `commit`'s site records as one Git commit by `actor`, then mirrors them into
/// PostgreSQL, judged again there; a refused change is taken back by a second commit, and a
/// database failure after judging is repaired from Git or leaves the tree marked out of step.
/// The caller holds the tree lock.
pub(crate) async fn commit_sites(
    state: &AppState,
    actor: &User,
    commit: SitesCommit,
) -> Result<(), AccessError> {
    let author = author_of(actor);
    let after: Vec<SiteRecord> = commit.changed.iter().map(|(_, a)| a.clone()).collect();
    let ids: Vec<NodeId> = after.iter().map(|r| NodeId(r.id)).collect();
    let settings: Vec<SiteSettings> = after
        .iter()
        .map(settings_of)
        .collect::<Result<_, _>>()
        .map_err(DbError::new)?;
    let store = state.checkpointer();
    write_sites(
        store,
        author.clone(),
        commit.message.clone(),
        Vec::new(),
        after,
        Vec::new(),
    )
    .await
    .map_err(git_failure)?;
    let undo = Undo {
        sites: commit
            .changed
            .iter()
            .filter_map(|(before, _)| before.clone())
            .collect(),
        removed_sites: commit
            .changed
            .iter()
            .filter(|(before, _)| before.is_none())
            .map(|(_, after)| after.id)
            .collect(),
        ..Undo::default()
    };
    let applied = state
        .db()
        .apply_sites_mirror(SitesMirror {
            settings: &settings,
            actor: actor.id,
            needs_admin: commit.needs_admin,
            needs: &commit.needs,
            also_lock: &commit.also_lock,
            release_hub: commit.release_hub,
            events: commit.events,
        })
        .await;
    match applied {
        Ok(()) => Ok(()),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: {} is in Git but not in the database: {error}",
                commit.message
            );
            if !repair_sites(state, &ids).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: {} could not be judged, so it is taken back: {error}",
                    commit.message
                );
            }
            if !take_back_all(state, author, &commit.message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// One event about `site` by `actor`.
pub(crate) fn site_event(
    kind: &'static str,
    actor: &User,
    site: NodeId,
    detail: serde_json::Value,
) -> NewEvent {
    NewEvent {
        kind,
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: Some(site),
        detail,
    }
}

/// Makes `site` a hub with the navigation bar `navigation`, or changes the bar of a hub, for
/// `actor`, an enabled administrator. One sites commit, mirrored with `hub.registered` the
/// first time and `navigation.changed` (`"which": "hub"`) after; the same bar again changes
/// nothing. The bar is checked by [`Db::validate_navigation`] and `actor` must be able to view
/// every node it targets ([`Db::check_navigation_saver`]).
///
/// # Errors
/// `Forbidden` for anyone else (`Archived` for them on an archived site they can see);
/// `NotFound` unless `site` is a live site; `Archived` on an archived site; `IsHub` for a site
/// that joined a hub; `InvalidNavigation`; `OutOfStep`; otherwise the Git or database error.
pub async fn set_hub(
    state: &AppState,
    actor: &User,
    site: NodeId,
    navigation: Vec<NavItem>,
) -> Result<(), AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    admin_on(db, actor, site).await?;
    let (node, access) = seen_site(state, actor, site).await?;
    if access.archived() {
        return Err(AccessError::Archived);
    }
    db.validate_navigation(&navigation, NavScope::Hub).await?;
    db.check_navigation_saver(actor.id, &navigation).await?;
    let (stored, before) = record_and_start(state, site).await?;
    if before.joined_hub.is_some() {
        return Err(AccessError::IsHub);
    }
    let wanted = HubRecord { navigation };
    if before.hub.as_ref() == Some(&wanted) {
        return Ok(());
    }
    let event = if before.hub.is_none() {
        site_event("hub.registered", actor, site, json!({ "name": node.name }))
    } else {
        site_event(
            "navigation.changed",
            actor,
            site,
            json!({ "which": "hub", "name": node.name }),
        )
    };
    let mut after = before;
    after.hub = Some(wanted);
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: format!("Set the hub navigation of {}", node.name),
            changed: vec![(stored, after)],
            needs_admin: true,
            needs: Vec::new(),
            also_lock: Vec::new(),
            release_hub: None,
            events: vec![event],
        },
    )
    .await
}

/// Unregisters the hub `site` for `actor`, an enabled administrator: one sites commit writing
/// its record with no hub and every member's with no joined hub, mirrored with
/// `hub.unregistered` and `hub.left` (`"released": true`) for each member.
///
/// # Errors
/// As [`set_hub`], with `NotAHub` for a site that is not a hub.
pub async fn unregister_hub(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<(), AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    admin_on(db, actor, site).await?;
    let (node, access) = seen_site(state, actor, site).await?;
    if access.archived() {
        return Err(AccessError::Archived);
    }
    let (stored, before) = record_and_start(state, site).await?;
    if before.hub.is_none() {
        return Err(AccessError::NotAHub);
    }
    let members = released_members(state, site).await?;
    let mut after = before;
    after.hub = None;
    let mut changed = vec![(stored, after)];
    changed.extend(members.into_iter().map(|(b, a)| (Some(b), a)));
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: format!("Unregister hub {}", node.name),
            changed,
            needs_admin: true,
            needs: Vec::new(),
            also_lock: Vec::new(),
            release_hub: Some(site),
            events: vec![site_event(
                "hub.unregistered",
                actor,
                site,
                json!({ "name": node.name }),
            )],
        },
    )
    .await
}

/// Joins `site` to the hub `hub`, or with `None` leaves the hub it joined, for `actor`, who
/// needs `manage_site` on the site and `view` on the hub. Joining never changes anyone's
/// access. One sites commit, mirrored with `hub.left` for a hub left and `hub.joined` for the
/// one joined, both site rows locked, lower id first; the same hub again changes nothing.
///
/// # Errors
/// `NotFound` unless `site` is a live site `actor` can see, or `hub` one they can view;
/// `Forbidden` (`Archived` on an archived site) without `manage_site`; `IsHub` when `site` is
/// a hub; `NotAHub` when `hub` is not one; `OutOfStep`; otherwise the Git or database error.
pub async fn join_hub(
    state: &AppState,
    actor: &User,
    site: NodeId,
    hub: Option<NodeId>,
) -> Result<(), AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let (node, access) = seen_site(state, actor, site).await?;
    require(&access, Perms::MANAGE_SITE)?;
    let (stored, before) = record_and_start(state, site).await?;
    if before.joined_hub == hub.map(|h| h.0) {
        return Ok(());
    }
    let mut events = Vec::new();
    if let Some(old) = before.joined_hub {
        events.push(site_event(
            "hub.left",
            actor,
            site,
            json!({ "name": node.name, "hub": old }),
        ));
    }
    let mut needs = vec![(site, Perms::MANAGE_SITE)];
    if let Some(hub) = hub {
        let (hub_node, hub_access) = seen_site(state, actor, hub).await?;
        if !hub_access.perms.contains(Perms::VIEW) {
            return Err(AccessError::NotFound);
        }
        if before.hub.is_some() {
            return Err(AccessError::IsHub);
        }
        let (hub_record, _) = record_and_start(state, hub).await?;
        if hub_record.is_none_or(|record| record.hub.is_none()) {
            return Err(AccessError::NotAHub);
        }
        needs.push((hub, Perms::VIEW));
        events.push(site_event(
            "hub.joined",
            actor,
            site,
            json!({ "name": node.name, "hub": hub.0, "hub_name": hub_node.name }),
        ));
    }
    let mut after = before;
    after.joined_hub = hub.map(|h| h.0);
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: match hub {
                Some(_) => format!("Join {} to a hub", node.name),
                None => format!("{} leaves its hub", node.name),
            },
            changed: vec![(stored, after)],
            needs_admin: false,
            needs,
            also_lock: hub.into_iter().collect(),
            release_hub: None,
            events,
        },
    )
    .await
}

/// Sets `site`'s own navigation to `items`, or with `None` makes it automatic (every library
/// of the site, by name), for `actor`, who needs `manage_site` on it. The items are checked by
/// [`Db::validate_navigation`] (node targets in the site itself) and `actor` must be able to
/// view every node they target. One sites commit, mirrored with `navigation.changed`
/// (`"which": "local"`); the same navigation again changes nothing.
///
/// # Errors
/// `NotFound` unless `site` is a live site `actor` can see; `Forbidden` (`Archived` on an
/// archived site) without `manage_site`; `InvalidNavigation`; `OutOfStep`; otherwise the Git
/// or database error.
pub async fn set_navigation(
    state: &AppState,
    actor: &User,
    site: NodeId,
    items: Option<Vec<NavItem>>,
) -> Result<(), AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let (node, access) = seen_site(state, actor, site).await?;
    require(&access, Perms::MANAGE_SITE)?;
    if let Some(items) = &items {
        db.validate_navigation(items, NavScope::Local(site)).await?;
        db.check_navigation_saver(actor.id, items).await?;
    }
    let (stored, before) = record_and_start(state, site).await?;
    if before.navigation == items {
        return Ok(());
    }
    let mut after = before;
    after.navigation = items;
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: format!("Change the navigation of {}", node.name),
            changed: vec![(stored, after)],
            needs_admin: false,
            needs: vec![(site, Perms::MANAGE_SITE)],
            also_lock: Vec::new(),
            release_hub: None,
            events: vec![site_event(
                "navigation.changed",
                actor,
                site,
                json!({ "which": "local", "name": node.name }),
            )],
        },
    )
    .await
}

/// Sets the organisation's global navigation to `items` for `actor`, an enabled
/// administrator: one sites commit writing `org/navigation.json`, mirrored with
/// `navigation.changed` (`"which": "global"`), taken back if refused. The items are checked by
/// [`Db::validate_navigation`] and `actor` must be able to view every node they target. The
/// same navigation again changes nothing.
///
/// # Errors
/// `Forbidden` for anyone else; `InvalidNavigation`; `OutOfStep`; otherwise the Git or
/// database error.
pub async fn set_global_navigation(
    state: &AppState,
    actor: &User,
    items: Vec<NavItem>,
) -> Result<(), AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    if !db
        .user(actor.id)
        .await?
        .is_some_and(|u| u.is_admin && !u.disabled)
    {
        return Err(AccessError::Forbidden);
    }
    db.validate_navigation(&items, NavScope::Global).await?;
    db.check_navigation_saver(actor.id, &items).await?;
    let store = Arc::clone(state.checkpointer());
    let stored = tokio::task::spawn_blocking(move || store.read_org_navigation())
        .await
        .map_err(|e| git_failure(CheckpointError::Git(GitError::new(e))))?
        .map_err(git_failure)?;
    let before = stored.unwrap_or(OrgNavigation {
        schema: SITE_SCHEMA,
        navigation: Vec::new(),
    });
    if before.navigation == items {
        return Ok(());
    }
    let after = OrgNavigation {
        schema: SITE_SCHEMA,
        navigation: items.clone(),
    };
    let author = author_of(actor);
    let message = "Change the global navigation".to_owned();
    write_records(
        state.checkpointer(),
        author.clone(),
        message.clone(),
        Undo {
            org_navigation: Some(after),
            ..Undo::default()
        },
    )
    .await
    .map_err(git_failure)?;
    let event = NewEvent {
        kind: "navigation.changed",
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: None,
        detail: json!({ "which": "global" }),
    };
    match db.apply_global_navigation_by(actor.id, &items, event).await {
        Ok(()) => Ok(()),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: the global navigation is in Git but not in the database: \
                 {error}"
            );
            // The next change reconciles the global navigation with Git (see `ensure_in_step`).
            state.tree_out_of_step().store(true, Ordering::SeqCst);
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: a change of the global navigation could not be judged, \
                     so it is taken back: {error}"
                );
            }
            let undo = Undo {
                org_navigation: Some(before),
                ..Undo::default()
            };
            if !take_back_all(state, author, &message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}
