//! Libraries' own settings: their kind (document, image, template or asset), description,
//! whether they take folders, the file types they accept and refuse, and the largest file they
//! take. Changing them is one Git commit of the library's record (`libraries/<xx>/<id>.json`,
//! moving `refs/sharecraft/sites` as site records do), then mirrored into PostgreSQL, taken back
//! if refused; the records are compared with Git at every start (see [`reconcile`], run by
//! [`crate::sites::reconcile`]).
//!
//! A library made before library records has no record in Git: it reads as a document library
//! with the defaults, and its record is written at its first change.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use collab_core::ids::{DocId, NodeId};
use records::{
    AccessError, Db, DbError, LibraryChange, LibraryKind, LibrarySettings, NewEvent, Node,
    NodeKind, Perms, TemplateFile, User, validate_library_settings,
};
use revisions::{CheckpointError, GitError, LibraryRecord};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::checkpoint::Checkpointer;
use crate::tree::{
    TreeError, Undo, author_of, ensure_in_step, git_failure, require, take_back_all, visible,
    write_records,
};

/// What a person asks to change of a library: each field given is set. `allowed_types` and
/// `max_file_bytes` given as `None` set them to every type and no limit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryEdit {
    /// What the library is for; 0 to 500 characters.
    pub description: Option<String>,
    /// `document`, `image`, `template` or `asset`.
    pub kind: Option<String>,
    /// Whether new folders may be made in it.
    pub folders: Option<bool>,
    /// The file types it accepts; `Some(None)` for every type.
    pub allowed_types: Option<Option<Vec<String>>>,
    /// The file types it refuses.
    pub blocked_types: Option<Vec<String>>,
    /// The largest file it takes, in bytes; `Some(None)` for no limit of its own.
    pub max_file_bytes: Option<Option<u64>>,
}

/// What a reconcile of the library records did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibrariesReport {
    /// Libraries in the tree (not purged).
    pub libraries: usize,
    /// Rows written because they differed from Git.
    pub updated: usize,
    /// Records that could not be written, and why.
    pub failed: Vec<(Uuid, String)>,
}

/// The record of library `id` in Git, if it has one.
pub(crate) async fn read_library_record(
    store: &Arc<dyn Checkpointer>,
    id: NodeId,
) -> Result<Option<LibraryRecord>, CheckpointError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || store.read_library_record(id.0))
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))?
}

/// Every library record in Git.
pub(crate) async fn read_libraries(
    store: &Arc<dyn Checkpointer>,
) -> Result<Vec<LibraryRecord>, CheckpointError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || store.read_libraries())
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))?
}

/// A library record as PostgreSQL holds it.
fn settings_of(record: &LibraryRecord) -> Result<LibrarySettings, String> {
    LibrarySettings::from_record(record).ok_or_else(|| {
        format!(
            "library {} has an unknown kind {:?}",
            record.id, record.kind
        )
    })
}

/// A list of extensions as given, each trimmed and lower-cased (`" .PNG"` is `".png"`).
fn normalized(types: Vec<String>) -> Vec<String> {
    types
        .into_iter()
        .map(|each| each.trim().to_lowercase())
        .collect()
}

/// The library `library` as `actor` sees it, with their access: `NotFound` unless it is a
/// library they can view that is not deleted.
async fn seen_library(
    db: &Db,
    actor: &User,
    library: NodeId,
) -> Result<(Node, records::Access), AccessError> {
    let (node, access) = visible(db, actor, library).await?;
    if node.kind != NodeKind::Library || node.deleted || !access.perms.contains(Perms::VIEW) {
        return Err(AccessError::NotFound);
    }
    Ok((node, access))
}

/// The library `library` with its settings, for `actor`, who needs `view` on it.
///
/// # Errors
/// `NotFound` when it is missing, deleted, not a library or not visible to `actor`; otherwise
/// the database error.
pub async fn show(
    state: &AppState,
    actor: &User,
    library: NodeId,
) -> Result<(Node, LibrarySettings), AccessError> {
    let db = state.db();
    let (node, _) = seen_library(db, actor, library).await?;
    let settings = db
        .library_settings(library)
        .await?
        .unwrap_or_else(|| LibrarySettings::defaults(library));
    Ok((node, settings))
}

/// The asset libraries `actor` can view, by name, each with its settings.
///
/// # Errors
/// The database error.
pub async fn assets(
    state: &AppState,
    actor: &User,
) -> Result<Vec<(Node, LibrarySettings)>, AccessError> {
    let db = state.db();
    let mut out = Vec::new();
    for library in db.asset_libraries().await? {
        match show(state, actor, library).await {
            Ok(found) => out.push(found),
            Err(AccessError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// The "New from template" choices of `library` for `actor`, who needs `view` on it: the
/// files in the template libraries of its site that `actor` can view and whose content is
/// available, by name. Files of another site, in a library of another kind, in the recycle
/// bin, or held or quarantined by the malware scanner are never among them.
///
/// # Errors
/// `NotFound` when the library is missing, deleted, not a library or not visible to `actor`;
/// otherwise the database error.
pub async fn templates(
    state: &AppState,
    actor: &User,
    library: NodeId,
) -> Result<Vec<TemplateFile>, AccessError> {
    let db = state.db();
    let (node, _) = seen_library(db, actor, library).await?;
    let mut viewable = Vec::new();
    for file in db.template_files(node.site).await? {
        let can_view = db
            .effective_access(actor.id, NodeId(file.id.0))
            .await?
            .is_some_and(|access| access.perms.contains(Perms::VIEW));
        if can_view {
            viewable.push(file);
        }
    }
    let ids: Vec<DocId> = viewable.iter().map(|file| file.id).collect();
    let held = db.scan_marks(&ids).await?;
    viewable.retain(|file| !held.contains_key(&file.id));
    Ok(viewable)
}

/// Changes `library`'s kind or settings for `actor`, who needs `manage_site` on it; making it
/// an asset library, or changing an asset library to another kind, needs an enabled
/// administrator too. Under the tree lock, the change is written to Git as one commit of the
/// library's record, then mirrored into PostgreSQL with `library.changed` (the fields changed,
/// each `{from, to}`), judged again there. A refused change is taken back by a second commit.
/// Values equal to the current ones change nothing. A library with no record in Git gets one.
/// Changing the kind never touches the files already in the library. Answers the settings as
/// PostgreSQL then holds them.
///
/// Extensions are taken trimmed and lower-cased.
///
/// # Errors
/// `NotFound` when the library is missing, deleted or not visible to `actor`; `Forbidden`
/// without `manage_site`, or for the asset kind without an administrator (`Archived` on an
/// archived site); `InvalidLibrarySettings` with the rule broken (see
/// [`records::validate_library_settings`]); `OutOfStep` while the tree needs repair; otherwise
/// the Git or database error.
pub async fn change(
    state: &AppState,
    actor: &User,
    library: NodeId,
    edit: LibraryEdit,
) -> Result<LibrarySettings, AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let (node, access) = seen_library(db, actor, library).await?;
    require(&access, Perms::MANAGE_SITE)?;
    let admin = db
        .user(actor.id)
        .await?
        .is_some_and(|u| u.is_admin && !u.disabled);
    let store = state.checkpointer();
    let stored = read_library_record(store, library)
        .await
        .map_err(git_failure)?;
    let before = stored
        .clone()
        .unwrap_or_else(|| LibraryRecord::defaults(library.0));

    let mut after = before.clone();
    let mut fields = Map::new();
    let mut note = |field: &str, from: Value, to: Value| {
        if from != to {
            fields.insert(field.to_owned(), json!({ "from": from, "to": to }));
        }
    };
    if let Some(kind) = edit.kind {
        note("kind", json!(after.kind), json!(kind));
        after.kind = kind;
    }
    if let Some(description) = edit.description {
        note("description", json!(after.description), json!(description));
        after.description = description;
    }
    if let Some(folders) = edit.folders {
        note("folders", json!(after.folders), json!(folders));
        after.folders = folders;
    }
    if let Some(allowed) = edit.allowed_types {
        let allowed = allowed.map(normalized);
        note("allowed_types", json!(after.allowed_types), json!(allowed));
        after.allowed_types = allowed;
    }
    if let Some(blocked) = edit.blocked_types {
        let blocked = normalized(blocked);
        note("blocked_types", json!(after.blocked_types), json!(blocked));
        after.blocked_types = blocked;
    }
    if let Some(bytes) = edit.max_file_bytes {
        note("max_file_bytes", json!(after.max_file_bytes), json!(bytes));
        after.max_file_bytes = bytes;
    }
    // Only administrators set the asset kind, or change a library away from it; the other
    // settings of an asset library are its managers' to change.
    let kind_changed = after.kind != before.kind;
    let needs_admin = kind_changed && (after.kind == "asset" || before.kind == "asset");
    if needs_admin && !admin {
        return Err(AccessError::Forbidden);
    }
    validate_library_settings(&after, admin || !kind_changed)?;
    if fields.is_empty() {
        return Ok(db
            .library_settings(library)
            .await?
            .unwrap_or_else(|| LibrarySettings::defaults(library)));
    }

    let author = author_of(actor);
    let message = format!("Change library {}", node.name);
    write_records(
        store,
        author.clone(),
        message.clone(),
        Undo {
            libraries: vec![after.clone()],
            ..Undo::default()
        },
    )
    .await
    .map_err(git_failure)?;
    let undo = Undo {
        libraries: stored.clone().into_iter().collect(),
        removed_libraries: if stored.is_none() {
            vec![library.0]
        } else {
            Vec::new()
        },
        ..Undo::default()
    };
    let settings = settings_of(&after).map_err(DbError::new)?;
    let applied = db
        .apply_library_change(LibraryChange {
            settings: &settings,
            actor: Some(actor.id),
            needs_admin,
            needs: Perms::MANAGE_SITE,
            events: vec![NewEvent {
                kind: "library.changed",
                actor: Some(actor.id),
                subject: None,
                document: None,
                node: Some(library),
                detail: json!({ "name": node.name, "fields": fields }),
            }],
        })
        .await;
    match applied {
        Ok(()) => Ok(db
            .library_settings(library)
            .await?
            .unwrap_or_else(|| LibrarySettings::defaults(library))),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: change to library {} is in Git but not in the database: \
                 {error}",
                library.0
            );
            if !repair_libraries(state, &[library]).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: change to library {} could not be judged, so it is \
                     taken back: {error}",
                    library.0
                );
            }
            if !take_back_all(state, author, &message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// Reconciles the rows of `libraries` from Git once. Returns whether every one is now in step.
pub(crate) async fn repair_libraries(state: &AppState, libraries: &[NodeId]) -> bool {
    if libraries.is_empty() {
        return true;
    }
    let db = state.db();
    let current: HashMap<NodeId, LibrarySettings> = match db.all_library_settings().await {
        Ok(rows) => rows.into_iter().map(|row| (row.id, row)).collect(),
        Err(e) => {
            eprintln!("sharecraft-server: could not repair libraries: {e}");
            return false;
        }
    };
    for library in libraries {
        let record = match read_library_record(state.checkpointer(), *library).await {
            Ok(record) => record.unwrap_or_else(|| LibraryRecord::defaults(library.0)),
            Err(CheckpointError::Unsupported) => LibraryRecord::defaults(library.0),
            Err(e) => {
                eprintln!(
                    "sharecraft-server: could not read library {} from Git: {e}",
                    library.0
                );
                return false;
            }
        };
        if let Err(e) = repair_library(db, &record, current.get(library)).await {
            eprintln!(
                "sharecraft-server: could not repair library {}: {e}",
                library.0
            );
            return false;
        }
    }
    true
}

/// Writes `record` into PostgreSQL if `current` differs from it, with `library.changed` marked
/// as a repair. Returns whether it wrote.
async fn repair_library(
    db: &Db,
    record: &LibraryRecord,
    current: Option<&LibrarySettings>,
) -> Result<bool, String> {
    let settings = settings_of(record)?;
    if current == Some(&settings) {
        return Ok(false);
    }
    db.apply_library_change(LibraryChange {
        settings: &settings,
        actor: None,
        needs_admin: false,
        needs: Perms::NONE,
        events: vec![NewEvent {
            kind: "library.changed",
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

/// Brings PostgreSQL's mirror of the library records up to date with Git: every library of the
/// tree (not purged) whose row is missing or differs is written from its record, or for a
/// library with none from the defaults, with `library.changed` marked `"repaired": true`. A
/// record that cannot be written, or whose library is not in the tree, does not stop the
/// others; it is listed in the report. A purged library's record and row stay as they are.
///
/// # Errors
/// The Git error if the records cannot be read (`Unsupported` for a store without them), or
/// the database error if the rows cannot be listed.
pub async fn reconcile(
    store: Arc<dyn Checkpointer>,
    db: &Db,
) -> Result<LibrariesReport, TreeError> {
    let records = read_libraries(&store).await?;
    let rows = db.node_rows().await?;
    let libraries: HashSet<NodeId> = rows
        .iter()
        .filter(|row| row.kind == NodeKind::Library && row.purged_at.is_none())
        .map(|row| row.id)
        .collect();
    let purged: HashSet<NodeId> = rows
        .iter()
        .filter(|row| row.kind == NodeKind::Library && row.purged_at.is_some())
        .map(|row| row.id)
        .collect();
    let current: HashMap<NodeId, LibrarySettings> = db
        .all_library_settings()
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    let mut report = LibrariesReport {
        libraries: libraries.len(),
        ..LibrariesReport::default()
    };
    let mut by_library: HashMap<NodeId, LibraryRecord> = HashMap::new();
    for record in records {
        let id = NodeId(record.id);
        if libraries.contains(&id) {
            by_library.insert(id, record);
        } else if !purged.contains(&id) {
            report
                .failed
                .push((record.id, "the library is not in the tree".to_owned()));
        }
    }
    let mut wanted: Vec<LibraryRecord> = libraries
        .iter()
        .map(|library| {
            by_library
                .remove(library)
                .unwrap_or_else(|| LibraryRecord::defaults(library.0))
        })
        .collect();
    wanted.sort_by_key(|record| record.id);
    for record in &wanted {
        match repair_library(db, record, current.get(&NodeId(record.id))).await {
            Ok(true) => report.updated = report.updated.saturating_add(1),
            Ok(false) => {}
            Err(error) => report.failed.push((record.id, error)),
        }
    }
    Ok(report)
}

/// The kind of library named `word`, for a new library; `InvalidLibrarySettings` for an
/// unknown one.
///
/// # Errors
/// As above.
pub fn parse_kind(word: &str) -> Result<LibraryKind, AccessError> {
    LibraryKind::parse(word).ok_or(AccessError::InvalidLibrarySettings("invalid_kind"))
}
