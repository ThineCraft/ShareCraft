//! Sites' own settings: changing them (one Git commit with the site's renamed tree record when
//! the name changes too, then mirrored into PostgreSQL, taken back if refused), and the
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
use records::{
    AccessError, Db, DbError, NewEvent, NodeKind, Perms, RegionalDefaults, SiteChange, SiteKind,
    SiteSettings, TemplateRow, User, validate_description, validate_language, validate_node_name,
    validate_time_zone,
};
use revisions::{
    CheckpointError, GitError, OrgNavigation, SITE_SCHEMA, SiteRecord, TemplateRecord,
};
use serde_json::{Map, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::AppState;
use crate::checkpoint::Checkpointer;
use crate::tree::{
    TreeError, Undo, author_of, ensure_in_step, git_failure, git_record, repair_ids, require,
    take_back_all, to_row, visible, write_sites,
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
            "{} sites, {} templates, {} updated, {} failed",
            self.sites,
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
        if let Err(e) = repair_site(db, &record, current.get(site)).await {
            eprintln!("sharecraft-server: could not repair site {}: {e}", site.0);
            return false;
        }
    }
    true
}

/// Writes `record` into PostgreSQL if `current` differs from it, with `site.changed` marked as
/// a repair. Returns whether it wrote.
async fn repair_site(
    db: &Db,
    record: &SiteRecord,
    current: Option<&SiteSettings>,
) -> Result<bool, String> {
    let settings = settings_of(record)?;
    if current.is_some_and(|now| now.same_record(&settings)) {
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

/// Brings PostgreSQL's mirror of the site records, saved templates and global navigation up to
/// date with Git: every site of the tree (not purged) whose row is missing or differs is
/// written from its record, or for a site with none from the organisation's current defaults,
/// with `site.changed` marked `"repaired": true` (hubs before the sites that joined them);
/// likewise each saved template (`template.saved`) and the global navigation
/// (`navigation.changed`). A record that cannot be written, or whose site is not in the tree,
/// does not stop the others; it is listed in the report.
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
        match repair_site(db, record, current.get(&NodeId(record.id))).await {
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
    Ok(report)
}
