//! Changes to the content tree (create, rename, move, copy, delete, and the recycle bin's
//! restore and purge), the start-up reconcile of PostgreSQL's mirror with Git, and adopting
//! documents recorded before the tree.
//!
//! Every change is judged under the server's tree lock, recorded as one Git commit credited to
//! the person who made it, then mirrored into PostgreSQL in one transaction that locks the
//! rows concerned and judges the person's access again. Git is the authority: each record is
//! built from the node's current record in Git, never from PostgreSQL.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use collab_core::ids::{DocId, NodeId, UserId};
use records::{
    Access, AccessError, BinStage, ClaimId, Db, DbError, Document, Editor, NewEvent, Node,
    NodeChange, NodeKind, NodeRow, NodesApplied, Perms, User, validate_document_name,
    validate_node_name,
};
use revisions::{
    Author, BlobRef, Checkpoint, CheckpointError, Content, GitError, Stamp, TreeChange, TreeMove,
    TreeRecord,
};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::AppState;
use crate::blobs::Held;
use crate::checkpoint::Checkpointer;
use crate::projection::record_first;
use crate::room::credited_name;

/// The format version of the tree records this server writes.
const TREE_SCHEMA: u32 = 1;
/// The name of the library every new site gets.
const LIBRARY_NAME: &str = "Documents";
/// The name of the site existing documents are adopted into.
const HOME_SITE: &str = "Home";
/// The author id of changes the server makes on its own.
const SERVER_AUTHOR_ID: &str = "sharecraft";
/// The author name of changes the server makes on its own.
const SERVER_AUTHOR_NAME: &str = "ShareCraft";
/// Longest node name, in characters.
const MAX_NAME_CHARS: usize = 120;
/// Characters a node name may not hold.
const REFUSED_CHARS: [char; 9] = ['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// One change to the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeOp {
    /// Creates a site (no parent), a library (in a site) or a folder (in a library or
    /// folder) with the identifier `id`. Files are created with [`create_document`].
    Create {
        parent: Option<NodeId>,
        kind: NodeKind,
        name: String,
        id: NodeId,
    },
    /// Renames a node.
    Rename { id: NodeId, name: String },
    /// Moves a folder or file into the library or folder `parent`.
    Move { id: NodeId, parent: NodeId },
    /// Deletes a file, or a folder with every live item below it, to the first stage of the
    /// site's recycle bin: one entry, `id`, for all of them.
    Delete { id: NodeId },
    /// Restores the recycle bin entry `id`, with everything deleted with it, where it was (see
    /// [`apply`]).
    Restore { id: NodeId },
    /// Moves the recycle bin entry `id` from the first stage to the second.
    ToSecondStage { id: NodeId },
    /// Purges the recycle bin entry `id`, in the second stage, with everything deleted with it.
    Purge { id: NodeId },
    /// Copies the file or folder `id` (a folder with every live item below it the person may
    /// view) into the library or folder `parent`, named `name` or as it is (see [`apply`]).
    Copy {
        id: NodeId,
        parent: NodeId,
        name: Option<String>,
    },
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The node the change is about, as PostgreSQL then holds it: for a copy, the new one.
    pub node: Node,
    /// Every file the change deleted: their open sessions end.
    pub deleted: Vec<DocId>,
    /// A restored item whose name was taken meanwhile, and the name it had; or a copy whose
    /// name had to change, and the name asked for.
    pub renamed: Option<(NodeId, String)>,
}

/// Why reading or repairing the tree, or adopting documents, failed.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// Git could not be read or written.
    #[error("Git: {0}")]
    Git(#[from] CheckpointError),
    /// The database failed.
    #[error("database: {0}")]
    Db(#[from] DbError),
    /// Documents wait to be adopted, and nobody could own them.
    #[error(
        "existing documents need an administrator to own them, and there is no enabled \
         administrator; add one with `sharecraft-server add-user --admin --email <email> \
         --name <name>` and start the server again"
    )]
    NoAdministrator,
}

/// What a reconcile of the tree did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeReport {
    /// Nodes found in Git.
    pub nodes: usize,
    /// Rows written because they differed from Git.
    pub updated: usize,
    /// Nodes that could not be written, and why.
    pub failed: Vec<(Uuid, String)>,
}

impl fmt::Display for TreeReport {
    /// One line with the counts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} nodes in Git, {} updated, {} failed",
            self.nodes,
            self.updated,
            self.failed.len()
        )
    }
}

/// What adopting existing documents did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Adopted {
    /// The Home site the documents are in; `None` if there is none (the run only moved grants
    /// or gave groups to sites that had none).
    pub site: Option<NodeId>,
    /// The Home site's Documents library; `None` as for `site`.
    pub library: Option<NodeId>,
    /// Documents placed in the tree by this run.
    pub documents: usize,
    /// Of those, the documents whose name had to change.
    pub renamed: usize,
    /// Documents that became scopes because they had grants of their own.
    pub scopes: u64,
    /// Grants moved onto the tree.
    pub grants: u64,
}

impl fmt::Display for Adopted {
    /// One line with the counts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} documents adopted into {HOME_SITE}, {} renamed, {} grants moved",
            self.documents, self.renamed, self.grants
        )
    }
}

/// The take-back of a refused change that could not be written to Git. It is written before
/// the next reconcile, which would otherwise copy the refused change from Git into PostgreSQL.
pub(crate) struct PendingUndo {
    author: Author,
    message: String,
    records: Vec<TreeRecord>,
}

/// A change worked out and judged, ready to be committed and mirrored.
struct Plan {
    /// The node the change is about.
    id: NodeId,
    message: String,
    /// The records to write, parents before children.
    records: Vec<TreeRecord>,
    /// The records that take the change back: the previous records, or the new ones deleted.
    undo: Vec<TreeRecord>,
    needs_admin: bool,
    needs: Vec<(NodeId, Perms)>,
    /// A new site: its creator becomes a member of its Owners group.
    site_groups: bool,
    document: Option<Editor>,
    events: Vec<NewEvent>,
    /// How many of the last `needs` are places below a folder being deleted.
    needs_below: usize,
    /// `needs` are judged as if the nodes were live (see [`Db::access_as_if_live`]).
    as_if_live: bool,
    /// The files the change deletes.
    deleted: Vec<DocId>,
    /// A restored item that had to be renamed, and its name before.
    renamed: Option<(NodeId, String)>,
    /// The files a copy makes, each with its editor.
    copies: Vec<(NodeId, Editor)>,
    /// The versions a copy stores once its tree commit is in, already claimed.
    contents: Option<CopiedContents>,
}

/// The first versions of the files a copy makes, claimed before the copy's tree commit and
/// committed together after it, and what is recorded once they are, or done if they cannot be.
struct CopiedContents {
    /// What to write for each file that has a version.
    versions: Vec<CopiedVersion>,
    /// The claims on their contents, in the same order.
    claims: Vec<ClaimId>,
    /// What the commit message names.
    name: String,
    /// The `node.copied` events, recorded once the versions are committed.
    events: Vec<NewEvent>,
    /// The tree commit's message and the records that take it back (the copies purged).
    message: String,
    undo: Vec<TreeRecord>,
}

/// The first version of one file a copy makes.
struct CopiedVersion {
    doc: DocId,
    file_name: String,
    blob: BlobRef,
    fields: Map<String, Value>,
}

impl Default for Plan {
    /// A change of nothing, for filling in the fields a change leaves as they are.
    fn default() -> Self {
        Plan {
            id: NodeId(Uuid::nil()),
            message: String::new(),
            records: Vec::new(),
            undo: Vec::new(),
            needs_admin: false,
            needs: Vec::new(),
            site_groups: false,
            document: None,
            events: Vec::new(),
            needs_below: 0,
            as_if_live: false,
            deleted: Vec::new(),
            renamed: None,
            copies: Vec::new(),
            contents: None,
        }
    }
}

/// Applies `op` for `actor`: judges it under the tree lock (see the module documentation),
/// writes one Git commit, then mirrors it into PostgreSQL with its event. Returns the node as
/// PostgreSQL then holds it. A rename to the same name or a move to the same place changes
/// nothing.
///
/// # Errors
/// `NotFound` when the node, or a parent or destination, is missing, deleted or not visible
/// to `actor`; `Forbidden` when they lack a permission (a site needs an administrator, a
/// library or a rename of one `manage_site` on the site, a folder `add` on the place it goes
/// into, a rename `edit`, a move `edit` on what moves and `add` on where it goes, a deletion
/// `delete`, a folder's deletion also on every place below it with permissions of its own,
/// `ForbiddenBelow` otherwise); `InvalidName`, `WrongExtension` or `NameTaken` for a name;
/// `WrongPlace` for a kind that cannot go there or cannot be moved or deleted (and for files,
/// which [`create_document`] creates); `IntoItself` for a move into the node or below it;
/// `IsSite` for deleting a site; `Exists` for an identifier already used; `OutOfStep` while the
/// tree needs repair; otherwise the Git or database error.
///
/// A deletion takes a folder with every live item below it to the first stage of the site's
/// recycle bin, as one entry, in one commit, and answers every file it deleted. The recycle
/// bin's operations take an entry (`NotFound` for anything else, or for an entry the person
/// may not see): restoring from the first stage and moving on to the second need `delete` on
/// the entry as if it were live ([`Db::access_as_if_live`]); restoring from the second stage
/// and purging (second stage only, `WrongPlace` otherwise) need `manage_site` on the site.
///
/// A restore puts the entry and everything deleted with it back where it was, with its own
/// permissions as they were (links revoked by the deletion stay revoked). A folder above it
/// still in the bin is restored too, that folder alone (`ParentInBin` unless the person may
/// restore it); what was deleted with such a folder stays in the bin, as entries of its own.
/// Under a purged folder the entry goes to the top of its library. A name taken meanwhile gets
/// ` (2)`, ` (3)` … before its extension, and the answer says so. A purge erases, once it has
/// committed, each content its files' versions named that nothing names any more.
///
/// A copy needs `view` on what is copied and `add` on where it goes (`IntoItself` for a folder
/// into itself or below itself). It makes new items with new identifiers that inherit their
/// access from where they go (no permissions or links are copied), holding the latest saved
/// version of each file and nothing of its history: the folder (with every live item below it
/// the person may view) in one tree commit, then the first versions of all its files in one
/// checkpoint commit, their contents claimed before either. Refused characters in the name
/// become `_` and a name taken there gets ` (2)`, ` (3)` …; the answer says so. View on every
/// item copied is judged again with the change, so view lost meanwhile refuses it. A file Git
/// holds without a document row is left out. If the versions cannot be committed after the
/// items were, the copy is taken back (its items purged, in no bin) and the Git error is
/// answered; its `node.copied` events are recorded only once the versions are committed.
pub async fn apply(state: &AppState, actor: &User, op: TreeOp) -> Result<Applied, AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let id = match &op {
        TreeOp::Create { id, .. }
        | TreeOp::Rename { id, .. }
        | TreeOp::Move { id, .. }
        | TreeOp::Delete { id }
        | TreeOp::Restore { id }
        | TreeOp::ToSecondStage { id }
        | TreeOp::Purge { id }
        | TreeOp::Copy { id, .. } => *id,
    };
    let plan = match op {
        TreeOp::Create {
            parent,
            kind,
            name,
            id,
        } => plan_create(state, actor, parent, kind, &name, id).await?,
        TreeOp::Rename { id, name } => plan_rename(state, actor, id, &name).await?,
        TreeOp::Move { id, parent } => plan_move(state, actor, id, parent).await?,
        TreeOp::Delete { id } => Some(plan_delete(state, actor, id).await?),
        TreeOp::Restore { id } => Some(plan_restore(state, actor, id).await?),
        TreeOp::ToSecondStage { id } => Some(plan_to_second_stage(state, actor, id).await?),
        TreeOp::Purge { id } => Some(plan_purge(state, Some(actor), id).await?),
        TreeOp::Copy { id, parent, name } => {
            Some(plan_copy(state, actor, id, parent, name.as_deref()).await?)
        }
    };
    let (deleted, renamed, id) = match plan {
        Some(mut plan) => {
            let deleted = std::mem::take(&mut plan.deleted);
            let renamed = plan.renamed.take();
            let contents = plan.contents.take();
            let target = plan.id;
            match commit(state, Some(actor), plan).await {
                Ok(done) => erase_released(state, done).await,
                Err(e) => {
                    if let Some(contents) = contents {
                        state.blobs().settle_all(db, &contents.claims, None).await;
                    }
                    return Err(e);
                }
            }
            if let Some(contents) = contents {
                store_copied(state, actor, contents).await?;
            }
            (deleted, renamed, target)
        }
        None => (Vec::new(), None, id),
    };
    let node = db.node(id).await?.ok_or(AccessError::NotFound)?;
    Ok(Applied {
        node,
        deleted,
        renamed,
    })
}

/// Purges, as the server, every recycle bin entry deleted [`records::BIN_DAYS`] days or more
/// before `now`, whatever its stage: one entry per tree turn, commit and transaction. Answers
/// how many it purged; a failure is logged and the entry left for the next sweep.
pub async fn purge_due(state: &AppState, now: OffsetDateTime) -> usize {
    let due = match state.db().due_for_purge(now).await {
        Ok(due) => due,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return 0;
        }
    };
    let mut purged = 0;
    for id in due {
        let state = state.clone();
        let done = tokio::spawn(async move {
            let _turn = state.tree_lock().lock().await;
            ensure_in_step(&state).await?;
            let plan = plan_purge(&state, None, id).await?;
            let done = commit(&state, None, plan).await?;
            erase_released(&state, done).await;
            Ok::<_, AccessError>(())
        })
        .await;
        match done {
            Ok(Ok(())) => purged += 1,
            // Purged or restored meanwhile.
            Ok(Err(AccessError::NotFound)) => {}
            Ok(Err(e)) => eprintln!("sharecraft-server: could not purge node {}: {e}", id.0),
            Err(e) => eprintln!("sharecraft-server: a purge did not finish: {e}"),
        }
    }
    if purged > 0 {
        println!("sharecraft-server: purged {purged} recycle bin item(s)");
    }
    purged
}

/// Creates the file `id` named `name`, opened in `editor`, in the library or folder `parent`:
/// its tree record in one Git commit, then its node and `documents` row with one
/// `document.created` event (saying it was uploaded, with its size, when `uploaded` gives one).
/// The file has no grants of its own: it inherits.
///
/// # Errors
/// As [`apply`] for a create: `NotFound`, `Forbidden` unless `actor` has `add` on
/// `parent`, `WrongPlace` unless `parent` is a library or folder, `InvalidName`,
/// `WrongExtension`, `NameTaken`, `Exists`, `OutOfStep`, or the Git or database error.
pub async fn create_document(
    state: &AppState,
    actor: &User,
    parent: NodeId,
    id: DocId,
    name: &str,
    editor: Editor,
    uploaded: Option<u64>,
) -> Result<Document, AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let name = new_document_name(db, actor, parent, name, editor).await?;
    let node = NodeId(id.0);
    unused(state, node).await?;
    let parent_record = git_record(state, parent).await?;
    let record = new_record(
        node,
        NodeKind::Document,
        Some(parent),
        parent_record.site,
        &name,
        author_of(actor),
    );
    let detail = match uploaded {
        Some(bytes) => json!({
            "name": name,
            "editor": editor.id(),
            "uploaded": true,
            "bytes": bytes,
        }),
        None => json!({ "name": name, "editor": editor.id() }),
    };
    let plan = Plan {
        id: node,
        message: format!("Create file {name}"),
        undo: vec![deleted(&record, author_of(actor))],
        records: vec![record],
        needs_admin: false,
        needs: vec![(parent, Perms::ADD)],
        site_groups: false,
        document: Some(editor),
        events: vec![NewEvent {
            kind: "document.created",
            actor: Some(actor.id),
            subject: None,
            document: Some(id),
            node: Some(node),
            detail,
        }],
        ..Plan::default()
    };
    commit(state, Some(actor), plan).await?;
    db.document(id)
        .await?
        .ok_or_else(|| DbError::new("the new document was not recorded").into())
}

/// Checks that `actor` may create a file named `name`, opened in `editor`, in `parent` now:
/// the checks [`create_document`] makes first, without taking the tree's turn. An upload makes
/// them before reading its body, so a request that would be refused stores nothing;
/// [`create_document`] makes them again.
///
/// # Errors
/// `NotFound`, `Forbidden` unless `actor` has `add` on `parent`, `WrongPlace`, `InvalidName`,
/// `WrongExtension`, `NameTaken`, or the database error.
pub async fn check_create_document(
    state: &AppState,
    actor: &User,
    parent: NodeId,
    name: &str,
    editor: Editor,
) -> Result<(), AccessError> {
    new_document_name(state.db(), actor, parent, name, editor)
        .await
        .map(|_| ())
}

/// `name`, or the first of `name (2)`, `name (3)` … (before the extension) that no undeleted
/// item in `parent` has, ignoring case: the name to suggest when `name` is taken there.
///
/// # Errors
/// The database error.
pub async fn suggest_name(
    db: &Db,
    parent: NodeId,
    name: &str,
    editor: Editor,
) -> Result<String, DbError> {
    let taken: HashSet<String> = db.live_child_names(parent).await?.into_iter().collect();
    Ok(unique_name(name, editor, &taken))
}

/// The name a new file `name` in `parent` would have, once `actor` is found to have `add` on
/// `parent`, a library or folder, and the name to be acceptable for `editor` and free there.
async fn new_document_name(
    db: &Db,
    actor: &User,
    parent: NodeId,
    name: &str,
    editor: Editor,
) -> Result<String, AccessError> {
    let name = validate_document_name(name, editor)?.to_owned();
    let parent_node = visible(db, actor, parent).await?;
    if !matches!(parent_node.0.kind, NodeKind::Library | NodeKind::Folder) {
        return Err(AccessError::WrongPlace);
    }
    require(&parent_node.1, Perms::ADD)?;
    if !db.name_free(parent, &name, None).await? {
        return Err(AccessError::NameTaken);
    }
    Ok(name)
}

/// What moving the tree records into their sub-folders did, as the start-up line says it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moved(pub TreeMove);

impl fmt::Display for Moved {
    /// One clause: how many records moved, and how many were found at both places.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let TreeMove { moved, duplicates } = self.0;
        if moved == 0 && duplicates == 0 {
            return f.write_str("no records to move into sub-folders");
        }
        write!(f, "{moved} records moved into sub-folders")?;
        if duplicates > 0 {
            write!(
                f,
                ", {duplicates} found at both places, the later of each kept"
            )?;
        }
        Ok(())
    }
}

/// Moves the tree records still at `tree/<id>.json`, where servers before the sub-folders
/// wrote them, into `tree/<xx>/<id>.json`: one commit by "ShareCraft" that moves
/// `refs/sharecraft/tree` and no document ref. Runs at start, before [`reconcile`]; a second
/// start moves nothing. The move is one way: back up the library before upgrading.
///
/// # Errors
/// The Git error (`Unsupported` for a store without a tree).
pub async fn move_records(store: Arc<dyn Checkpointer>) -> Result<Moved, TreeError> {
    let author = server_author();
    let moved = tokio::task::spawn_blocking(move || store.move_tree_records(&author))
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))??;
    Ok(Moved(moved))
}

/// Brings PostgreSQL's nodes up to date with Git: every record whose row is missing or
/// differs (kind, parent, site, name, whether it is deleted, and its recycle bin state: what
/// it was deleted with, its stage, whether it is purged) is written, parents before
/// children, with the matching `node.*` event marked `"repaired": true`. A record that cannot
/// be written, or that does not lead up to a site, does not stop the others; it is listed in
/// the report.
///
/// # Errors
/// The Git error if the tree cannot be read (`Unsupported` for a store without a tree), or the
/// database error if the nodes cannot be listed.
pub async fn reconcile(store: Arc<dyn Checkpointer>, db: &Db) -> Result<TreeReport, TreeError> {
    let records = read_tree(&store).await?;
    let existing: HashMap<NodeId, NodeRow> = db
        .node_rows()
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    let mut report = TreeReport {
        nodes: records.len(),
        ..TreeReport::default()
    };
    let (ordered, orphans) = parents_first(&records);
    for record in orphans {
        report.failed.push((
            record.id,
            "the node does not lead up to a site in Git".to_owned(),
        ));
    }
    let mut failed: HashSet<Uuid> = report.failed.iter().map(|(id, _)| *id).collect();
    for record in ordered {
        if record.parent.is_some_and(|p| failed.contains(&p)) {
            failed.insert(record.id);
            report
                .failed
                .push((record.id, "its parent could not be written".to_owned()));
            continue;
        }
        match repair(db, record, existing.get(&NodeId(record.id))).await {
            Ok(true) => report.updated = report.updated.saturating_add(1),
            Ok(false) => {}
            Err(error) => {
                failed.insert(record.id);
                report.failed.push((record.id, error));
            }
        }
    }
    Ok(report)
}

/// Places the documents recorded before the content tree into it, and moves their grants onto
/// it. Runs only when a document has no node, a grant is still in `document_access`, or a site
/// lacks its three groups; otherwise returns `None`.
///
/// Documents without a node go into the Documents library of the Home site, both created (by
/// "ShareCraft", in the same commit as the documents' records) unless an earlier adoption made
/// them. A refused character in a name becomes `_`; a name already used in the library gets
/// ` (2)`, ` (3)` … before its extension; the `documents` row takes the new name. Then, in one
/// transaction, the nodes are written, every site lacking its groups gets them (with no
/// members: administrators, who have every permission, fill them), every document with grants
/// of its own becomes a scope with those grants, and one `tree.adopted` event records the
/// counts.
///
/// If the commit was written and the transaction was not, the next start's [`reconcile`]
/// writes the nodes and this moves the grants.
///
/// # Errors
/// `NoAdministrator`, before anything is written, if no enabled administrator exists; the Git
/// or database error otherwise.
pub async fn adopt_flat_documents(
    store: Arc<dyn Checkpointer>,
    db: &Db,
) -> Result<Option<Adopted>, TreeError> {
    if !db.adoption_pending().await? {
        return Ok(None);
    }
    let users = db.users().await?;
    if !users.iter().any(|u| u.is_admin && !u.disabled) {
        return Err(TreeError::NoAdministrator);
    }
    let unplaced = db.unplaced_documents().await?;
    let tree = read_tree(&store).await?;
    let home = find_home(&tree);
    if unplaced.is_empty() {
        let done = db
            .finish_adoption(&[], home.map(|(site, _)| site), 0, 0)
            .await
            .map_err(access_to_db)?;
        return Ok(Some(Adopted {
            site: home.map(|(site, _)| site),
            library: home.map(|(_, library)| library),
            scopes: done.scopes,
            grants: done.grants,
            ..Adopted::default()
        }));
    }

    let server = server_author();
    let now = now_text();
    let mut records = Vec::new();
    let (site, library, mut taken) = match home {
        Some((site, library)) => {
            let taken = tree
                .iter()
                .filter(|r| r.parent == Some(library.0) && r.deleted.is_none())
                .map(|r| r.name.to_lowercase())
                .collect();
            (site, library, taken)
        }
        None => {
            let site = NodeId(Uuid::new_v4());
            let library = NodeId(Uuid::new_v4());
            records.push(new_record(
                site,
                NodeKind::Site,
                None,
                site.0,
                HOME_SITE,
                server.clone(),
            ));
            records.push(new_record(
                library,
                NodeKind::Library,
                Some(site),
                site.0,
                LIBRARY_NAME,
                server.clone(),
            ));
            (site, library, HashSet::new())
        }
    };
    let names: HashMap<UserId, String> = users.into_iter().map(|u| (u.id, u.name)).collect();
    let mut renamed = 0usize;
    for item in &unplaced {
        let document = &item.document;
        let name = unique_name(
            &clean_name(&document.name, document.editor),
            document.editor,
            &taken,
        );
        taken.insert(name.to_lowercase());
        if name != document.name {
            renamed = renamed.saturating_add(1);
        }
        let creator = document
            .created_by
            .and_then(|id| names.get(&id).map(|name| (id, name)))
            .map_or_else(server_author, |(id, name)| Author {
                id: id.0.to_string(),
                name: credited_name(name),
            });
        let mut record = new_record(
            NodeId(document.id.0),
            NodeKind::Document,
            Some(library),
            site.0,
            &name,
            creator,
        );
        record.created.at = item
            .created_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| now.clone());
        records.push(record);
    }
    let rows = records
        .iter()
        .map(to_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbError::new)?;
    let message = format!(
        "Adopt {} existing documents into {HOME_SITE}",
        unplaced.len()
    );
    write_tree(&store, server, message, records).await?;
    let done = db
        .finish_adoption(&rows, Some(site), unplaced.len(), renamed)
        .await
        .map_err(access_to_db)?;
    Ok(Some(Adopted {
        site: Some(site),
        library: Some(library),
        documents: unplaced.len(),
        renamed,
        scopes: done.scopes,
        grants: done.grants,
    }))
}

/// Refuses tree changes while the tree is out of step, unless the take-backs that could not be
/// written now are, and a reconcile of the whole tree then succeeds.
async fn ensure_in_step(state: &AppState) -> Result<(), AccessError> {
    let flag = state.tree_out_of_step();
    if !flag.load(Ordering::SeqCst) {
        return Ok(());
    }
    if !write_pending_undos(state).await {
        return Err(AccessError::OutOfStep);
    }
    match reconcile(Arc::clone(state.checkpointer()), state.db()).await {
        Ok(report) if report.failed.is_empty() => {
            flag.store(false, Ordering::SeqCst);
            Ok(())
        }
        Ok(report) => {
            eprintln!("sharecraft-server: the content tree is still out of step: {report}");
            Err(AccessError::OutOfStep)
        }
        Err(e) => {
            eprintln!("sharecraft-server: the content tree is still out of step: {e}");
            Err(AccessError::OutOfStep)
        }
    }
}

/// Writes `plan` to Git, then mirrors it into PostgreSQL. If PostgreSQL refuses it (the
/// person's access changed meanwhile, or a name was taken outside this server's lock), or the
/// database fails before the change was judged allowed (`Unjudged`), the commit is taken back
/// by a second one and the error answered; nothing is repaired from Git. If the database fails
/// after the change was judged allowed, the nodes are reconciled from Git once (a new file is
/// first taken back, as Git does not hold its editor); if that fails too, the tree is marked
/// out of step. A take-back that cannot be written also marks it out of step, and is kept to
/// be written before the next reconcile (see [`ensure_in_step`]).
///
/// `actor` is `None` for a change the server makes on its own (a purge when an item's time in
/// the bin is up), credited to "ShareCraft" and judged by nobody. Answers what PostgreSQL
/// released (see [`erase_released`]).
async fn commit(
    state: &AppState,
    actor: Option<&User>,
    plan: Plan,
) -> Result<NodesApplied, AccessError> {
    let store = state.checkpointer();
    let author = actor.map_or_else(server_author, author_of);
    let rows = plan
        .records
        .iter()
        .map(to_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbError::new)?;
    write_tree(
        store,
        author.clone(),
        plan.message.clone(),
        plan.records.clone(),
    )
    .await
    .map_err(git_failure)?;
    let actor_id = actor.map(|a| a.id);
    let applied = state
        .db()
        .apply_nodes(NodeChange {
            rows: &rows,
            events: plan.events,
            actor: actor_id,
            needs_admin: plan.needs_admin,
            needs: &plan.needs,
            site_groups: if plan.site_groups { actor_id } else { None },
            document: plan.document,
            copies: &plan.copies,
            needs_below: plan.needs_below,
            as_if_live: plan.as_if_live,
        })
        .await;
    match applied {
        Ok(done) => Ok(done),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: tree change to node {} is in Git but not in the database: \
                 {error}",
                plan.id.0
            );
            // Git does not hold a new file's editor: new files are taken back.
            let taken_back = (plan.document.is_none() && plan.copies.is_empty())
                || take_back(state, author.clone(), &plan.message, plan.undo).await;
            let ids: Vec<Uuid> = plan.records.iter().map(|r| r.id).collect();
            if !(taken_back && repair_ids(state, &ids).await) {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: tree change to node {} could not be judged, so it is \
                     taken back: {error}",
                    plan.id.0
                );
            }
            if !take_back(state, author, &plan.message, plan.undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// Erases, once a purge has committed, each content the versions of its files named that
/// nothing names any more (and no write holds), and adds to each of its `node.purged` events
/// the bytes this freed (`bytes_freed`). Nothing is erased while a flag says the content rows
/// may be incomplete; the sweeper erases it after the rebuild. Failures are logged.
async fn erase_released(state: &AppState, done: NodesApplied) {
    if done.purge_events.is_empty() && done.released.is_empty() {
        return;
    }
    let db = state.db();
    let mut freed: u64 = 0;
    let erasing = match db.blob_refs_flagged().await {
        Ok(flagged) => !flagged,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            false
        }
    };
    if erasing {
        for blob in &done.released {
            match state.blobs().erase_if_unreferenced(db, &blob.sha256).await {
                Ok(true) => freed = freed.saturating_add(blob.size),
                Ok(false) => {}
                Err(e) => eprintln!("sharecraft-server: cannot erase purged content: {e}"),
            }
        }
    }
    for event in done.purge_events {
        if let Err(e) = db
            .add_event_detail(event, "bytes_freed", json!(freed))
            .await
        {
            eprintln!("sharecraft-server: database failure: {e}");
        }
    }
}

/// Writes `undo` as a second commit, taking back the change `message` describes. Returns
/// whether it was written; if not, it is kept to be written before the next reconcile.
async fn take_back(state: &AppState, author: Author, message: &str, undo: Vec<TreeRecord>) -> bool {
    let pending = PendingUndo {
        author,
        message: format!("Undo: {message}"),
        records: undo,
    };
    match write_undo(state, &pending).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("sharecraft-server: could not take back a tree change in Git: {e}");
            pending_undos(state).push(pending);
            false
        }
    }
}

/// Writes the take-backs kept by [`take_back`], oldest first. Returns whether all were written;
/// the first that fails and those after it stay kept.
async fn write_pending_undos(state: &AppState) -> bool {
    let kept = std::mem::take(&mut *pending_undos(state));
    let mut kept = kept.into_iter();
    while let Some(pending) = kept.next() {
        if let Err(e) = write_undo(state, &pending).await {
            eprintln!("sharecraft-server: still could not take back a tree change in Git: {e}");
            let mut queue = pending_undos(state);
            let later = std::mem::take(&mut *queue);
            queue.push(pending);
            queue.extend(kept);
            queue.extend(later);
            return false;
        }
    }
    true
}

/// Writes one take-back as a commit.
async fn write_undo(state: &AppState, pending: &PendingUndo) -> Result<(), CheckpointError> {
    write_tree(
        state.checkpointer(),
        pending.author.clone(),
        pending.message.clone(),
        pending.records.clone(),
    )
    .await
}

/// The take-backs not yet written.
fn pending_undos(state: &AppState) -> std::sync::MutexGuard<'_, Vec<PendingUndo>> {
    state
        .tree_undos()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reconciles the nodes `ids` from Git once. Returns whether every one is now in step.
async fn repair_ids(state: &AppState, ids: &[Uuid]) -> bool {
    let db = state.db();
    for id in ids {
        let id = *id;
        let store = Arc::clone(state.checkpointer());
        let record = tokio::task::spawn_blocking(move || store.read_tree_record(id)).await;
        let record = match record {
            Ok(Ok(Some(record))) => record,
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => {
                eprintln!("sharecraft-server: could not read node {id} from Git: {e}");
                return false;
            }
            Err(e) => {
                eprintln!("sharecraft-server: could not read node {id} from Git: {e}");
                return false;
            }
        };
        let current = match db.node_rows().await {
            Ok(rows) => rows.into_iter().find(|row| row.id.0 == id),
            Err(e) => {
                eprintln!("sharecraft-server: could not repair node {id}: {e}");
                return false;
            }
        };
        if let Err(e) = repair(db, &record, current.as_ref()).await {
            eprintln!("sharecraft-server: could not repair node {id}: {e}");
            return false;
        }
    }
    true
}

/// Writes `record` into PostgreSQL if `current` differs from it, with the matching event marked
/// as a repair. Returns whether it wrote.
async fn repair(db: &Db, record: &TreeRecord, current: Option<&NodeRow>) -> Result<bool, String> {
    let row = to_row(record)?.normalized();
    let kind = match current {
        None => "node.created",
        Some(now) if same(now, &row) => return Ok(false),
        Some(now) if now.purged_at.is_none() && row.purged_at.is_some() => "node.purged",
        Some(now) if now.deleted_at.is_some() && row.deleted_at.is_none() => "node.created",
        Some(now) if now.deleted_at.is_none() && row.deleted_at.is_some() => "node.deleted",
        Some(now) if now.parent != row.parent || now.site != row.site => "node.moved",
        Some(now) if now.name != row.name => "node.renamed",
        Some(now) if row.bin_stage == Some(2) && now.bin_stage != Some(2) => "node.binned",
        Some(_) => "node.deleted",
    };
    let event = NewEvent {
        kind,
        actor: None,
        subject: None,
        document: document_of(row.kind, row.id),
        node: Some(row.id),
        detail: json!({
            "repaired": true,
            "name": row.name,
            "parent": row.parent.map(|p| p.0),
        }),
    };
    db.apply_node(&row, Some(event))
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Whether PostgreSQL's row says what Git's does, recycle bin state included.
fn same(now: &NodeRow, git: &NodeRow) -> bool {
    let (now, git) = (now.normalized(), git.normalized());
    now.kind == git.kind
        && now.parent == git.parent
        && now.site == git.site
        && now.name == git.name
        && now.deleted_at.is_some() == git.deleted_at.is_some()
        && now.deleted_with == git.deleted_with
        && now.bin_stage == git.bin_stage
        && now.purged_at.is_some() == git.purged_at.is_some()
}

/// `records` ordered so each parent comes before its children, and the records that do not
/// lead up to a site (a missing parent, or a cycle).
fn parents_first(records: &[TreeRecord]) -> (Vec<&TreeRecord>, Vec<&TreeRecord>) {
    let mut children: HashMap<Option<Uuid>, Vec<&TreeRecord>> = HashMap::new();
    for record in records {
        children.entry(record.parent).or_default().push(record);
    }
    let mut ordered = Vec::with_capacity(records.len());
    let mut next = children.remove(&None).unwrap_or_default();
    while !next.is_empty() {
        let mut below = Vec::new();
        for record in next {
            ordered.push(record);
            below.extend(children.remove(&Some(record.id)).unwrap_or_default());
        }
        next = below;
    }
    let orphans = children.into_values().flatten().collect();
    (ordered, orphans)
}

async fn plan_create(
    state: &AppState,
    actor: &User,
    parent: Option<NodeId>,
    kind: NodeKind,
    name: &str,
    id: NodeId,
) -> Result<Option<Plan>, AccessError> {
    let db = state.db();
    let name = validate_node_name(name)?.to_owned();
    let author = author_of(actor);
    if kind == NodeKind::Site {
        if parent.is_some() {
            return Err(AccessError::WrongPlace);
        }
        let current = db.user(actor.id).await?;
        if !current.is_some_and(|u| u.is_admin && !u.disabled) {
            return Err(AccessError::Forbidden);
        }
        if !db.site_name_free(&name, None).await? {
            return Err(AccessError::NameTaken);
        }
        unused(state, id).await?;
        let library = NodeId(Uuid::new_v4());
        let site = new_record(id, NodeKind::Site, None, id.0, &name, author.clone());
        let lib = new_record(
            library,
            NodeKind::Library,
            Some(id),
            id.0,
            LIBRARY_NAME,
            author.clone(),
        );
        let undo = vec![deleted(&site, author.clone()), deleted(&lib, author)];
        let events = [&site, &lib]
            .iter()
            .map(|r| created_event(actor.id, r))
            .collect();
        return Ok(Some(Plan {
            id,
            message: format!("Create site {name}"),
            records: vec![site, lib],
            undo,
            needs_admin: true,
            needs: Vec::new(),
            site_groups: true,
            document: None,
            events,
            ..Plan::default()
        }));
    }
    let parent = parent.ok_or(AccessError::WrongPlace)?;
    let (parent_node, access) = visible(db, actor, parent).await?;
    let (fits, needed) = match kind {
        NodeKind::Library => (parent_node.kind == NodeKind::Site, Perms::MANAGE_SITE),
        NodeKind::Folder => (
            matches!(parent_node.kind, NodeKind::Library | NodeKind::Folder),
            Perms::ADD,
        ),
        NodeKind::Site | NodeKind::Document => (false, Perms::ADD),
    };
    if !fits {
        return Err(AccessError::WrongPlace);
    }
    require(&access, needed)?;
    if !db.name_free(parent, &name, None).await? {
        return Err(AccessError::NameTaken);
    }
    unused(state, id).await?;
    let parent_record = git_record(state, parent).await?;
    let record = new_record(
        id,
        kind,
        Some(parent),
        parent_record.site,
        &name,
        author.clone(),
    );
    Ok(Some(Plan {
        id,
        message: format!("Create {} {name}", kind_word(kind)),
        undo: vec![deleted(&record, author)],
        events: vec![created_event(actor.id, &record)],
        records: vec![record],
        needs_admin: false,
        needs: vec![(parent, needed)],
        site_groups: false,
        document: None,
        ..Plan::default()
    }))
}

async fn plan_rename(
    state: &AppState,
    actor: &User,
    id: NodeId,
    name: &str,
) -> Result<Option<Plan>, AccessError> {
    let db = state.db();
    let (node, access) = visible(db, actor, id).await?;
    let needed = match node.kind {
        NodeKind::Site | NodeKind::Library => Perms::MANAGE_SITE,
        NodeKind::Folder | NodeKind::Document => Perms::EDIT,
    };
    require(&access, needed)?;
    let name = match db.document(DocId(id.0)).await? {
        Some(document) if node.kind == NodeKind::Document => {
            validate_document_name(name, document.editor)?
        }
        _ => validate_node_name(name)?,
    }
    .to_owned();
    let before = git_record(state, id).await?;
    if before.name == name {
        return Ok(None);
    }
    let free = match node.parent {
        None => db.site_name_free(&name, Some(id)).await?,
        Some(parent) => db.name_free(parent, &name, Some(id)).await?,
    };
    if !free {
        return Err(AccessError::NameTaken);
    }
    let mut after = before.clone();
    after.name.clone_from(&name);
    Ok(Some(Plan {
        id,
        message: format!("Rename {} {} to {name}", kind_word(node.kind), before.name),
        events: vec![NewEvent {
            kind: "node.renamed",
            actor: Some(actor.id),
            subject: None,
            document: document_of(node.kind, id),
            node: Some(id),
            detail: json!({ "from": before.name, "to": name }),
        }],
        records: vec![after],
        undo: vec![before],
        needs_admin: false,
        needs: vec![(id, needed)],
        site_groups: false,
        document: None,
        ..Plan::default()
    }))
}

async fn plan_move(
    state: &AppState,
    actor: &User,
    id: NodeId,
    destination: NodeId,
) -> Result<Option<Plan>, AccessError> {
    let db = state.db();
    let (node, access) = visible(db, actor, id).await?;
    if !matches!(node.kind, NodeKind::Folder | NodeKind::Document) {
        return Err(AccessError::WrongPlace);
    }
    let (place, place_access) = visible(db, actor, destination).await?;
    if !matches!(place.kind, NodeKind::Library | NodeKind::Folder) {
        return Err(AccessError::WrongPlace);
    }
    require(&access, Perms::EDIT)?;
    require(&place_access, Perms::ADD)?;
    if db.is_at_or_below(destination, id).await? {
        return Err(AccessError::IntoItself);
    }
    if node.parent == Some(destination) {
        return Ok(None);
    }
    if !db.name_free(destination, &node.name, Some(id)).await? {
        return Err(AccessError::NameTaken);
    }
    let before = git_record(state, id).await?;
    let place_record = git_record(state, destination).await?;
    let mut after = before.clone();
    after.parent = Some(destination.0);
    after.site = place_record.site;
    let mut records = vec![after];
    let mut undo = vec![before.clone()];
    if place_record.site != before.site {
        // Everything below moves to the new site too.
        let tree = read_tree(state.checkpointer()).await.map_err(git_failure)?;
        for below in below(&tree, id.0) {
            let mut moved = below.clone();
            moved.site = place_record.site;
            undo.push(below.clone());
            records.push(moved);
        }
    }
    Ok(Some(Plan {
        id,
        message: format!(
            "Move {} {} to {}",
            kind_word(node.kind),
            node.name,
            place.name
        ),
        events: vec![NewEvent {
            kind: "node.moved",
            actor: Some(actor.id),
            subject: None,
            document: document_of(node.kind, id),
            node: Some(id),
            detail: json!({ "from": before.parent, "to": destination.0 }),
        }],
        records,
        undo,
        needs_admin: false,
        needs: vec![(destination, Perms::ADD), (id, Perms::EDIT)],
        site_groups: false,
        document: None,
        ..Plan::default()
    }))
}

/// Deletes `id`, and for a folder every live item below it, to the first stage of the site's
/// recycle bin: each record marked deleted now by `actor` and with `id`, `id` the entry. Items
/// below that were deleted before keep their own entries. A folder needs `delete` on itself and
/// on every live place below it with permissions of its own (`ForbiddenBelow` otherwise).
async fn plan_delete(state: &AppState, actor: &User, id: NodeId) -> Result<Plan, AccessError> {
    let db = state.db();
    let (node, access) = visible(db, actor, id).await?;
    match node.kind {
        NodeKind::Site => return Err(AccessError::IsSite),
        NodeKind::Library => return Err(AccessError::WrongPlace),
        NodeKind::Folder | NodeKind::Document => {}
    }
    require(&access, Perms::DELETE)?;
    let mut needs = vec![(id, Perms::DELETE)];
    let mut needs_below = 0;
    if node.kind == NodeKind::Folder {
        for scope in db.scopes_below(id).await? {
            let allowed = db
                .effective_access(actor.id, scope)
                .await?
                .is_some_and(|a| a.perms.contains(Perms::DELETE));
            if !allowed {
                return Err(AccessError::ForbiddenBelow);
            }
            needs.push((scope, Perms::DELETE));
            needs_below += 1;
        }
    }
    let before = git_record(state, id).await?;
    let mut undo = vec![before.clone()];
    if node.kind == NodeKind::Folder {
        let tree = read_tree(state.checkpointer()).await.map_err(git_failure)?;
        undo.extend(
            below(&tree, id.0)
                .into_iter()
                .filter(|r| r.deleted.is_none())
                .cloned(),
        );
    }
    let stamp = Stamp {
        at: now_text(),
        by: author_of(actor),
    };
    let records: Vec<TreeRecord> = undo
        .iter()
        .map(|r| {
            let mut gone = r.clone();
            gone.deleted = Some(stamp.clone());
            gone.deleted_with = Some(id.0);
            gone.bin_stage = (r.id == id.0).then_some(1);
            gone
        })
        .collect();
    let documents: Vec<DocId> = records
        .iter()
        .filter(|r| r.kind == kind_word(NodeKind::Document))
        .map(|r| DocId(r.id))
        .collect();
    let mut events = vec![NewEvent {
        kind: "node.deleted",
        actor: Some(actor.id),
        subject: None,
        document: document_of(node.kind, id),
        node: Some(id),
        detail: json!({ "name": node.name, "items": records.len() }),
    }];
    // Each file deleted with a folder has the deletion in its own history too.
    events.extend(
        records
            .iter()
            .filter(|r| r.id != id.0 && r.kind == kind_word(NodeKind::Document))
            .map(|r| NewEvent {
                kind: "node.deleted",
                actor: Some(actor.id),
                subject: None,
                document: Some(DocId(r.id)),
                node: Some(NodeId(r.id)),
                detail: json!({ "name": r.name, "with": id.0 }),
            }),
    );
    Ok(Plan {
        id,
        message: format!("Delete {} {}", kind_word(node.kind), node.name),
        events,
        records,
        undo,
        needs,
        needs_below,
        deleted: documents,
        ..Plan::default()
    })
}

/// The deletion a record belongs to: the entry it was deleted with (its own id for a record
/// deleted before the recycle bin); `None` for a live record.
fn deleted_with(record: &TreeRecord) -> Option<Uuid> {
    record
        .deleted
        .as_ref()
        .map(|_| record.deleted_with.unwrap_or(record.id))
}

/// The bin stage of the entry `record`; a record from before the bin is in the first.
fn stage_of(record: &TreeRecord) -> BinStage {
    match record.bin_stage {
        Some(2) => BinStage::Second,
        _ => BinStage::First,
    }
}

/// The recycle bin entry `id` as Git records it, if `id` is one: deleted, not purged, and the
/// item its deletion started from.
async fn bin_record(state: &AppState, id: NodeId) -> Result<TreeRecord, AccessError> {
    let record = git_record(state, id).await?;
    if record.purged.is_some() || deleted_with(&record) != Some(id.0) {
        return Err(AccessError::NotFound);
    }
    Ok(record)
}

/// What a person needs to restore or move on the entry `record` (or a folder deleted with it,
/// `node`), which they must be able to see: in the first stage `delete` on `node` as if it were
/// live, in the second `manage_site` on the site. Checked now (`NotFound` if they cannot see
/// it, `Forbidden` if they can and lack the permission); returned to be judged again when the
/// change is applied.
async fn bin_need(
    db: &Db,
    actor: &User,
    record: &TreeRecord,
    node: NodeId,
) -> Result<(NodeId, Perms), AccessError> {
    match stage_of(record) {
        BinStage::First => {
            let access = db
                .access_as_if_live(actor.id, node)
                .await?
                .ok_or(AccessError::NotFound)?;
            require(&access, Perms::DELETE)?;
            Ok((node, Perms::DELETE))
        }
        BinStage::Second => {
            let site = NodeId(record.site);
            let manages = db
                .effective_access(actor.id, site)
                .await?
                .is_some_and(|a| a.perms.contains(Perms::MANAGE_SITE));
            if !manages {
                return Err(AccessError::NotFound);
            }
            Ok((site, Perms::MANAGE_SITE))
        }
    }
}

/// `record` live again: no longer deleted, in no bin.
fn revived(record: &TreeRecord) -> TreeRecord {
    let mut back = record.clone();
    back.deleted = None;
    back.deleted_with = None;
    back.bin_stage = None;
    back.purged = None;
    back
}

/// Restores the entry `id` and everything deleted with it (see [`apply`]).
async fn plan_restore(state: &AppState, actor: &User, id: NodeId) -> Result<Plan, AccessError> {
    let db = state.db();
    let entry = bin_record(state, id).await?;
    let mut needs = vec![bin_need(db, actor, &entry, id).await?];
    let tree = read_tree(state.checkpointer()).await.map_err(git_failure)?;
    let by_id: HashMap<Uuid, &TreeRecord> = tree.iter().map(|r| (r.id, r)).collect();
    let mut group = vec![&entry];
    group.extend(
        below(&tree, id.0)
            .into_iter()
            .filter(|r| r.purged.is_none() && deleted_with(r) == Some(id.0)),
    );

    // The folders above it still in the bin, nearest first, up to the first live one; none
    // if a folder above it was purged, when it goes to the top of its library.
    let mut alone: Vec<&TreeRecord> = Vec::new();
    let mut purged_above = false;
    let mut next = entry.parent;
    while let Some(parent) = next {
        let record = by_id
            .get(&parent)
            .copied()
            .ok_or_else(|| git_failure(missing(parent)))?;
        if record.purged.is_some() {
            purged_above = true;
            break;
        }
        if record.deleted.is_none() {
            break;
        }
        alone.push(record);
        next = record.parent;
    }
    let mut new_parent = entry.parent;
    if purged_above {
        alone.clear();
        let mut up = entry.parent;
        new_parent = None;
        while let Some(at) = up {
            let record = by_id
                .get(&at)
                .copied()
                .ok_or_else(|| git_failure(missing(at)))?;
            if record.kind == kind_word(NodeKind::Library) {
                new_parent = Some(record.id);
                break;
            }
            up = record.parent;
        }
        if new_parent.is_none() {
            return Err(git_failure(missing(id.0)));
        }
    }
    for folder in &alone {
        let top = deleted_with(folder)
            .and_then(|with| by_id.get(&with).copied())
            .unwrap_or(folder);
        let need = bin_need(db, actor, top, NodeId(folder.id))
            .await
            .map_err(|e| match e {
                AccessError::NotFound | AccessError::Forbidden => AccessError::ParentInBin,
                other => other,
            })?;
        needs.push(need);
    }
    // Ancestors before descendants, each once.
    needs.reverse();
    let mut seen = HashSet::new();
    needs.retain(|(node, _)| seen.insert(*node));

    // What was deleted with a folder restored alone stays in the bin: each part of it whose
    // folder is live again becomes an entry of its own, in its entry's stage.
    let alone_ids: HashSet<Uuid> = alone.iter().map(|r| r.id).collect();
    let mut retopped = Vec::new();
    for folder in &alone {
        if deleted_with(folder) != Some(folder.id) {
            continue;
        }
        let stage = folder.bin_stage.unwrap_or(1);
        let left: Vec<&TreeRecord> = below(&tree, folder.id)
            .into_iter()
            .filter(|r| {
                r.purged.is_none()
                    && deleted_with(r) == Some(folder.id)
                    && !alone_ids.contains(&r.id)
            })
            .collect();
        let left_ids: HashSet<Uuid> = left.iter().map(|r| r.id).collect();
        for record in left {
            let mut top = record;
            while let Some(parent) = top.parent.filter(|p| left_ids.contains(p)) {
                match by_id.get(&parent) {
                    Some(above) => top = above,
                    None => break,
                }
            }
            let mut moved = record.clone();
            moved.deleted_with = Some(top.id);
            moved.bin_stage = (top.id == record.id).then_some(stage);
            retopped.push((record, moved));
        }
    }

    // The highest node restored is the one whose name may be taken now.
    let mut restored: Vec<TreeRecord> = alone.iter().rev().map(|r| revived(r)).collect();
    restored.extend(group.iter().map(|r| revived(r)));
    if let Some(first) = restored.first_mut() {
        if first.id == id.0 {
            first.parent = new_parent;
        }
        let parent = first.parent.map(NodeId).ok_or(AccessError::WrongPlace)?;
        let taken: HashSet<String> = db.live_child_names(parent).await?.into_iter().collect();
        if taken.contains(&first.name.to_lowercase()) {
            let editor = if first.kind == kind_word(NodeKind::Document) {
                db.document(DocId(first.id)).await?.map(|d| d.editor)
            } else {
                None
            };
            first.name = unique_node_name(&first.name, editor, &taken);
        }
    }
    let renamed = restored.first().and_then(|first| {
        let before = by_id.get(&first.id)?;
        (before.name != first.name).then(|| (NodeId(first.id), before.name.clone()))
    });

    let mut undo: Vec<TreeRecord> = alone.iter().rev().map(|r| (*r).clone()).collect();
    undo.extend(group.iter().map(|r| (*r).clone()));
    let mut events: Vec<NewEvent> = alone
        .iter()
        .rev()
        .map(|folder| NewEvent {
            kind: "node.restored",
            actor: Some(actor.id),
            subject: None,
            document: None,
            node: Some(NodeId(folder.id)),
            detail: json!({ "name": folder.name, "alone": true, "for": id.0 }),
        })
        .collect();
    let kind = parse_kind(&entry.kind).unwrap_or(NodeKind::Folder);
    let restored_entry = restored.iter().find(|r| r.id == id.0);
    events.push(NewEvent {
        kind: "node.restored",
        actor: Some(actor.id),
        subject: None,
        document: document_of(kind, id),
        node: Some(id),
        detail: json!({
            "name": restored_entry.map_or(&entry.name, |r| &r.name),
            "from_name": renamed.as_ref().map(|(_, from)| from),
            "parent": restored_entry.and_then(|r| r.parent),
            "items": group.len(),
            "folders_restored": alone.len(),
        }),
    });
    for (before, after) in retopped {
        undo.push(before.clone());
        restored.push(after);
    }
    Ok(Plan {
        id,
        message: format!("Restore {} {}", kind_word(kind), entry.name),
        records: restored,
        undo,
        needs,
        events,
        as_if_live: true,
        renamed,
        ..Plan::default()
    })
}

/// Moves the entry `id` from the first stage of the bin to the second.
async fn plan_to_second_stage(
    state: &AppState,
    actor: &User,
    id: NodeId,
) -> Result<Plan, AccessError> {
    let entry = bin_record(state, id).await?;
    let need = bin_need(state.db(), actor, &entry, id).await?;
    if stage_of(&entry) != BinStage::First {
        return Err(AccessError::WrongPlace);
    }
    let mut after = entry.clone();
    after.bin_stage = Some(2);
    let kind = parse_kind(&entry.kind).unwrap_or(NodeKind::Folder);
    Ok(Plan {
        id,
        message: format!(
            "Move {} {} to the second stage of the recycle bin",
            kind_word(kind),
            entry.name
        ),
        events: vec![NewEvent {
            kind: "node.binned",
            actor: Some(actor.id),
            subject: None,
            document: document_of(kind, id),
            node: Some(id),
            detail: json!({ "name": entry.name }),
        }],
        records: vec![after],
        undo: vec![entry],
        needs: vec![need],
        as_if_live: true,
        ..Plan::default()
    })
}

/// Purges the entry `id` with everything deleted with it: by `actor` from the second stage
/// (with `manage_site` on the site), or by the server (`None`) at any stage when its time in
/// the bin is up. Items below it deleted on their own keep their entries.
async fn plan_purge(
    state: &AppState,
    actor: Option<&User>,
    id: NodeId,
) -> Result<Plan, AccessError> {
    let entry = bin_record(state, id).await?;
    let needs = match actor {
        Some(actor) => {
            let need = bin_need(state.db(), actor, &entry, id).await?;
            if stage_of(&entry) != BinStage::Second {
                return Err(AccessError::WrongPlace);
            }
            vec![need]
        }
        None => Vec::new(),
    };
    let tree = read_tree(state.checkpointer()).await.map_err(git_failure)?;
    let mut undo = vec![entry.clone()];
    undo.extend(
        below(&tree, id.0)
            .into_iter()
            .filter(|r| r.purged.is_none() && deleted_with(r) == Some(id.0))
            .cloned(),
    );
    let stamp = Stamp {
        at: now_text(),
        by: actor.map_or_else(server_author, author_of),
    };
    let records: Vec<TreeRecord> = undo
        .iter()
        .map(|r| {
            let mut gone = r.clone();
            gone.purged = Some(stamp.clone());
            gone.bin_stage = None;
            gone
        })
        .collect();
    let kind = parse_kind(&entry.kind).unwrap_or(NodeKind::Folder);
    Ok(Plan {
        id,
        message: format!("Purge {} {}", kind_word(kind), entry.name),
        events: vec![NewEvent {
            kind: "node.purged",
            actor: actor.map(|a| a.id),
            subject: None,
            document: document_of(kind, id),
            node: Some(id),
            detail: json!({ "name": entry.name, "items": records.len() }),
        }],
        records,
        undo,
        needs,
        as_if_live: true,
        ..Plan::default()
    })
}

/// Copies `id` into `destination` for `actor` (see [`apply`]): the new records, their files,
/// the `node.copied` events naming the sources, and the latest version of each file, its
/// content held (an old inline version written to the store first) and then claimed.
async fn plan_copy(
    state: &AppState,
    actor: &User,
    id: NodeId,
    destination: NodeId,
    name: Option<&str>,
) -> Result<Plan, AccessError> {
    let db = state.db();
    let (node, access) = visible(db, actor, id).await?;
    if !matches!(node.kind, NodeKind::Folder | NodeKind::Document) {
        return Err(AccessError::WrongPlace);
    }
    let (place, place_access) = visible(db, actor, destination).await?;
    if !matches!(place.kind, NodeKind::Library | NodeKind::Folder) {
        return Err(AccessError::WrongPlace);
    }
    require(&access, Perms::VIEW)?;
    require(&place_access, Perms::ADD)?;
    if db.is_at_or_below(destination, id).await? {
        return Err(AccessError::IntoItself);
    }
    let tree = read_tree(state.checkpointer()).await.map_err(git_failure)?;
    let source = tree
        .iter()
        .find(|r| r.id == id.0 && r.deleted.is_none())
        .ok_or(AccessError::NotFound)?;
    let place_record = git_record(state, destination).await?;

    // The source, and every live item below it the person may view whose folder is copied.
    // Each with the id of its copy.
    let new_top = NodeId(Uuid::new_v4());
    let mut chosen: Vec<(&TreeRecord, Uuid)> = vec![(source, new_top.0)];
    let mut ids: HashMap<Uuid, Uuid> = HashMap::from([(source.id, new_top.0)]);
    for record in below(&tree, id.0) {
        if record.deleted.is_some() || !record.parent.is_some_and(|p| ids.contains_key(&p)) {
            continue;
        }
        let viewable = db
            .effective_access(actor.id, NodeId(record.id))
            .await?
            .is_some_and(|a| a.perms.contains(Perms::VIEW));
        if viewable {
            let fresh = Uuid::new_v4();
            ids.insert(record.id, fresh);
            chosen.push((record, fresh));
        }
    }
    let mut editors: HashMap<Uuid, Editor> = HashMap::new();
    for (record, _) in &chosen {
        if record.kind == kind_word(NodeKind::Document)
            && let Some(document) = db.document(DocId(record.id)).await?
        {
            editors.insert(record.id, document.editor);
        }
    }
    // A file Git holds with no document row cannot be opened, so it is not copied.
    chosen.retain(|(r, _)| r.kind != kind_word(NodeKind::Document) || editors.contains_key(&r.id));

    // The top item's name: as asked, or as it is, cleaned and made unique where it goes.
    let editor = editors.get(&source.id).copied();
    if node.kind == NodeKind::Document && editor.is_none() {
        return Err(AccessError::NotFound);
    }
    let asked = name.unwrap_or(&source.name).trim().to_owned();
    let cleaned = match editor {
        Some(editor) => {
            let cleaned = clean_name(&asked, editor);
            validate_document_name(&cleaned, editor)?;
            cleaned
        }
        None => clean_folder_name(&asked),
    };
    let taken: HashSet<String> = db
        .live_child_names(destination)
        .await?
        .into_iter()
        .collect();
    let top_name = unique_node_name(&cleaned, editor, &taken);
    let renamed = (top_name != asked).then(|| (new_top, asked.clone()));

    let author = author_of(actor);
    let mut records = Vec::with_capacity(chosen.len());
    for (record, fresh) in &chosen {
        let kind = parse_kind(&record.kind).unwrap_or(NodeKind::Folder);
        let (parent, name) = if record.id == source.id {
            (destination, top_name.as_str())
        } else {
            let parent = record.parent.and_then(|p| ids.get(&p)).copied();
            (
                NodeId(parent.ok_or_else(|| git_failure(missing(record.id)))?),
                record.name.as_str(),
            )
        };
        records.push(new_record(
            NodeId(*fresh),
            kind,
            Some(parent),
            place_record.site,
            name,
            author.clone(),
        ));
    }
    let copies: Vec<(NodeId, Editor)> = chosen
        .iter()
        .filter_map(|(r, fresh)| editors.get(&r.id).map(|e| (NodeId(*fresh), *e)))
        .collect();

    let mut events = vec![NewEvent {
        kind: "node.copied",
        actor: Some(actor.id),
        subject: None,
        document: document_of(node.kind, new_top),
        node: Some(new_top),
        detail: json!({
            "from": source.id,
            "name": top_name,
            "parent": destination.0,
            "items": records.len(),
        }),
    }];
    events.extend(
        chosen
            .iter()
            .filter(|(r, _)| r.id != source.id && editors.contains_key(&r.id))
            .map(|(r, fresh)| NewEvent {
                kind: "node.copied",
                actor: Some(actor.id),
                subject: None,
                document: Some(DocId(*fresh)),
                node: Some(NodeId(*fresh)),
                detail: json!({ "from": r.id, "name": r.name, "with": new_top.0 }),
            }),
    );

    // The latest version of each file, its content held until it is claimed.
    let mut versions = Vec::new();
    let mut held: Vec<Held> = Vec::new();
    for (record, fresh) in &chosen {
        if !editors.contains_key(&record.id) {
            continue;
        }
        let doc = DocId(record.id);
        let store = Arc::clone(state.checkpointer());
        let stored = tokio::task::spawn_blocking(move || store.load(doc))
            .await
            .map_err(|e| git_failure(CheckpointError::Git(GitError::new(e))))?
            .map_err(git_failure)?;
        let Some(stored) = stored else {
            continue;
        };
        let hold = match stored.content {
            Content::Blob(blob) => state.blobs().hold_stored(blob).await.ok_or_else(|| {
                DbError::new(format!(
                    "content {} of document {} is missing from the content store",
                    blob.hex(),
                    doc.0
                ))
            })?,
            Content::Inline(bytes) => state
                .blobs()
                .write_bytes(bytes)
                .await
                .map_err(DbError::new)?,
        };
        let new_doc = DocId(*fresh);
        let file_name = records
            .iter()
            .find(|r| r.id == new_doc.0)
            .map_or_else(|| record.name.clone(), |r| r.name.clone());
        versions.push(CopiedVersion {
            doc: new_doc,
            file_name,
            blob: hold.blob(),
            fields: stored.meta.fields,
        });
        held.push(hold);
    }
    let wanted: Vec<(BlobRef, DocId)> = versions.iter().map(|v| (v.blob, v.doc)).collect();
    let claims = state
        .blobs()
        .claim_all(db, &wanted)
        .await
        .map_err(DbError::new)?;
    // The claims keep the contents now.
    drop(held);

    let undo: Vec<TreeRecord> = records.iter().map(|r| deleted(r, author.clone())).collect();
    let message = format!(
        "Copy {} {} to {}",
        kind_word(node.kind),
        source.name,
        place.name
    );
    // View on everything copied is judged again with the change, ancestors first.
    let mut needs = vec![(destination, Perms::ADD), (id, Perms::VIEW)];
    needs.extend(
        chosen
            .iter()
            .skip(1)
            .map(|(r, _)| (NodeId(r.id), Perms::VIEW)),
    );
    Ok(Plan {
        id: new_top,
        message: message.clone(),
        records,
        undo: undo.clone(),
        needs,
        renamed,
        copies,
        contents: Some(CopiedContents {
            versions,
            claims,
            name: top_name,
            events,
            message,
            undo,
        }),
        ..Plan::default()
    })
}

/// Commits the first versions of the files a copy made, as one commit credited to `actor`,
/// then settles their claims, records the revisions and the `node.copied` events. If the
/// commit fails the claims are withdrawn, the copy is taken back (see [`take_back_copy`]) and
/// the Git error is answered.
async fn store_copied(
    state: &AppState,
    actor: &User,
    contents: CopiedContents,
) -> Result<(), AccessError> {
    let db = state.db();
    if contents.versions.is_empty() {
        record_events(db, &contents.events).await;
        return Ok(());
    }
    let author = author_of(actor);
    let message = format!("Copy {}", contents.name);
    let store = Arc::clone(state.checkpointer());
    let versions = contents.versions;
    let written = tokio::task::spawn_blocking(move || {
        let authors = [author.clone()];
        let checkpoints: Vec<Checkpoint<'_>> = versions
            .iter()
            .map(|v| Checkpoint {
                doc: v.doc,
                file_name: &v.file_name,
                blob: v.blob,
                base: None,
                authors: &authors,
                fields: &v.fields,
            })
            .collect();
        let written = store.write_new(&author, &message, &checkpoints);
        (written, versions)
    })
    .await
    .map_err(|e| git_failure(CheckpointError::Git(GitError::new(e))));
    let (revision, versions) = match written {
        Ok((Ok(revision), versions)) => (revision, versions),
        Ok((Err(e), _)) => {
            state.blobs().settle_all(db, &contents.claims, None).await;
            eprintln!("sharecraft-server: the versions of a copy could not be stored: {e}");
            take_back_copy(state, actor, contents.message, contents.undo).await;
            return Err(git_failure(e));
        }
        Err(e) => {
            state.blobs().settle_all(db, &contents.claims, None).await;
            take_back_copy(state, actor, contents.message, contents.undo).await;
            return Err(e);
        }
    };
    state
        .blobs()
        .settle_all(db, &contents.claims, Some(&revision))
        .await;
    for version in versions {
        record_first(
            state.checkpointer(),
            db,
            state.events(),
            version.doc,
            &revision,
        )
        .await;
    }
    record_events(db, &contents.events).await;
    Ok(())
}

/// Records `events` on their own, logging a failure.
async fn record_events(db: &Db, events: &[NewEvent]) {
    for event in events {
        if let Err(e) = db.append_event(event).await {
            eprintln!("sharecraft-server: database failure: {e}");
        }
    }
}

/// Takes back a copy whose versions could not be committed: `undo` (its records purged) as a
/// second commit, then mirrored into PostgreSQL, so the copies are tombstones that no list and
/// no bin shows. A take-back that cannot be written or mirrored marks the tree out of step, as
/// [`commit`] does.
async fn take_back_copy(state: &AppState, actor: &User, message: String, undo: Vec<TreeRecord>) {
    let ids: Vec<Uuid> = undo.iter().map(|r| r.id).collect();
    if !take_back(state, author_of(actor), &message, undo.clone()).await {
        state.tree_out_of_step().store(true, Ordering::SeqCst);
        return;
    }
    let rows = match undo.iter().map(to_row).collect::<Result<Vec<_>, _>>() {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("sharecraft-server: cannot take back a copy: {e}");
            state.tree_out_of_step().store(true, Ordering::SeqCst);
            return;
        }
    };
    let mirrored = state
        .db()
        .apply_nodes(NodeChange {
            rows: &rows,
            ..NodeChange::default()
        })
        .await;
    if let Err(e) = mirrored {
        eprintln!("sharecraft-server: cannot take back a copy in the database: {e}");
        if !repair_ids(state, &ids).await {
            state.tree_out_of_step().store(true, Ordering::SeqCst);
        }
    }
}

/// The Git error for a record that names a node Git does not hold.
fn missing(id: Uuid) -> CheckpointError {
    CheckpointError::InvalidTreeChange(format!("node {id} leads to a node not in the tree"))
}

/// `name`, or the first of `name (2)`, `name (3)` … that `taken` (lower-cased names) does not
/// hold: before the extension for a file opened in `editor`, at the end for a folder.
fn unique_node_name(name: &str, editor: Option<Editor>, taken: &HashSet<String>) -> String {
    if let Some(editor) = editor {
        return unique_name(name, editor, taken);
    }
    if !taken.contains(&name.to_lowercase()) {
        return name.to_owned();
    }
    (2u64..)
        .map(|n| fit(name, "", &format!(" ({n})")))
        .find(|candidate| !taken.contains(&candidate.to_lowercase()))
        .unwrap_or_else(|| name.to_owned())
}

/// Every record below `id` in `tree`, deleted ones included, parents before children.
fn below(tree: &[TreeRecord], id: Uuid) -> Vec<&TreeRecord> {
    let mut children: HashMap<Uuid, Vec<&TreeRecord>> = HashMap::new();
    for record in tree {
        if let Some(parent) = record.parent {
            children.entry(parent).or_default().push(record);
        }
    }
    let mut found = Vec::new();
    let mut seen = HashSet::from([id]);
    let mut next = vec![id];
    while let Some(parent) = next.pop() {
        for child in children.get(&parent).map(Vec::as_slice).unwrap_or_default() {
            if seen.insert(child.id) {
                found.push(*child);
                next.push(child.id);
            }
        }
    }
    found
}

/// The node `id` and `actor`'s access to it; `NotFound` unless they have some.
async fn visible(db: &Db, actor: &User, id: NodeId) -> Result<(Node, Access), AccessError> {
    let access = db
        .effective_access(actor.id, id)
        .await?
        .ok_or(AccessError::NotFound)?;
    let node = db.node(id).await?.ok_or(AccessError::NotFound)?;
    Ok((node, access))
}

/// `Forbidden` unless `access` holds every permission of `needed`.
fn require(access: &Access, needed: Perms) -> Result<(), AccessError> {
    if access.perms.contains(needed) {
        Ok(())
    } else {
        Err(AccessError::Forbidden)
    }
}

/// `Exists` if `id` is already used by a node, in Git or PostgreSQL, or by a document.
async fn unused(state: &AppState, id: NodeId) -> Result<(), AccessError> {
    let db = state.db();
    if db.node(id).await?.is_some() || db.document(DocId(id.0)).await?.is_some() {
        return Err(AccessError::Exists);
    }
    let store = Arc::clone(state.checkpointer());
    let in_git = tokio::task::spawn_blocking(move || store.read_tree_record(id.0))
        .await
        .map_err(|e| git_failure(CheckpointError::Git(GitError::new(e))))?
        .map_err(git_failure)?;
    if in_git.is_some() {
        return Err(AccessError::Exists);
    }
    Ok(())
}

/// The record of `id` in Git; `NotFound` if Git has none.
async fn git_record(state: &AppState, id: NodeId) -> Result<TreeRecord, AccessError> {
    let store = Arc::clone(state.checkpointer());
    tokio::task::spawn_blocking(move || store.read_tree_record(id.0))
        .await
        .map_err(|e| git_failure(CheckpointError::Git(GitError::new(e))))?
        .map_err(git_failure)?
        .ok_or(AccessError::NotFound)
}

/// Every record of the tree in Git.
async fn read_tree(store: &Arc<dyn Checkpointer>) -> Result<Vec<TreeRecord>, CheckpointError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || store.read_tree())
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))?
}

/// Writes `records` as one commit credited to `author`.
async fn write_tree(
    store: &Arc<dyn Checkpointer>,
    author: Author,
    message: String,
    records: Vec<TreeRecord>,
) -> Result<(), CheckpointError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || {
        store
            .write_tree(&TreeChange {
                author: &author,
                message: &message,
                records: &records,
            })
            .map(|_| ())
    })
    .await
    .map_err(|e| CheckpointError::Git(GitError::new(e)))?
}

/// A Git failure, answered as an unavailable store.
fn git_failure(e: CheckpointError) -> AccessError {
    AccessError::Db(DbError::new(format!("the content tree in Git: {e}")))
}

/// A refusal while adopting, which can only come from data the database already holds.
fn access_to_db(e: AccessError) -> TreeError {
    match e {
        AccessError::Db(e) => TreeError::Db(e),
        other => TreeError::Db(DbError::new(other)),
    }
}

/// The Home site and its Documents library made by an earlier adoption, if Git holds them.
fn find_home(tree: &[TreeRecord]) -> Option<(NodeId, NodeId)> {
    let by_server = |r: &&TreeRecord| r.created.by.id == SERVER_AUTHOR_ID && r.deleted.is_none();
    tree.iter()
        .filter(by_server)
        .filter(|r| r.kind == "site")
        .find_map(|site| {
            tree.iter()
                .filter(by_server)
                .find(|r| r.kind == "library" && r.parent == Some(site.id))
                .map(|library| (NodeId(site.id), NodeId(library.id)))
        })
}

/// `name` made acceptable as a node name: refused and control characters become `_`, as does
/// a trailing dot; an empty name becomes "Untitled"; a long one is shortened before its
/// extension.
fn clean_name(name: &str, editor: Editor) -> String {
    let replaced: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || REFUSED_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut cleaned = replaced.trim().to_owned();
    while cleaned.ends_with('.') {
        cleaned.pop();
        cleaned.push('_');
    }
    let (stem, extension) = split_extension(&cleaned, editor);
    let stem = if stem.trim().is_empty() {
        "Untitled"
    } else {
        stem
    };
    fit(stem, extension, "")
}

/// `name` made acceptable as a folder name: refused and control characters become `_`, as does
/// a trailing dot; an empty name becomes "Untitled"; a long one is shortened.
fn clean_folder_name(name: &str) -> String {
    let replaced: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || REFUSED_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut cleaned = replaced.trim().to_owned();
    while cleaned.ends_with('.') {
        cleaned.pop();
        cleaned.push('_');
    }
    if cleaned.is_empty() {
        return "Untitled".to_owned();
    }
    fit(&cleaned, "", "")
}

/// `name`, or the first of `name (2)`, `name (3)` … (before the extension) that `taken`, which
/// holds lower-cased names, does not hold.
fn unique_name(name: &str, editor: Editor, taken: &HashSet<String>) -> String {
    if !taken.contains(&name.to_lowercase()) {
        return name.to_owned();
    }
    let (stem, extension) = split_extension(name, editor);
    (2u64..)
        .map(|n| fit(stem, extension, &format!(" ({n})")))
        .find(|candidate| !taken.contains(&candidate.to_lowercase()))
        .unwrap_or_else(|| name.to_owned())
}

/// `stem`, `suffix` and `extension` joined, with `stem` shortened so the whole fits the name
/// length limit.
fn fit(stem: &str, extension: &str, suffix: &str) -> String {
    let room = MAX_NAME_CHARS
        .saturating_sub(extension.chars().count())
        .saturating_sub(suffix.chars().count())
        .max(1);
    let stem: String = stem.chars().take(room).collect();
    format!("{}{suffix}{extension}", stem.trim_end())
}

/// The name before its extension, and the extension with its dot: the editor's own if the
/// name ends with it, otherwise whatever follows the last dot (none for a leading dot).
fn split_extension(name: &str, editor: Editor) -> (&str, &str) {
    if let Some(ext) = editor.native_extension() {
        let start = name.len().saturating_sub(ext.len());
        if name.len() >= ext.len()
            && name
                .get(start..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(ext))
            && let Some(parts) = name.get(..start).zip(name.get(start..))
        {
            return parts;
        }
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

/// A new, undeleted record.
fn new_record(
    id: NodeId,
    kind: NodeKind,
    parent: Option<NodeId>,
    site: Uuid,
    name: &str,
    by: Author,
) -> TreeRecord {
    TreeRecord {
        schema: TREE_SCHEMA,
        id: id.0,
        kind: kind_word(kind).to_owned(),
        parent: parent.map(|p| p.0),
        site,
        name: name.to_owned(),
        created: Stamp { at: now_text(), by },
        deleted: None,
        bin_stage: None,
        deleted_with: None,
        purged: None,
    }
}

/// `record`, deleted and purged now by `by`: the take-back of a creation, which never reaches
/// the recycle bin.
fn deleted(record: &TreeRecord, by: Author) -> TreeRecord {
    let stamp = Stamp { at: now_text(), by };
    let mut gone = record.clone();
    gone.deleted = Some(stamp.clone());
    gone.deleted_with = Some(record.id);
    gone.bin_stage = None;
    gone.purged = Some(stamp);
    gone
}

/// The `node.created` event of `record`, made by `actor`.
fn created_event(actor: UserId, record: &TreeRecord) -> NewEvent {
    NewEvent {
        kind: "node.created",
        actor: Some(actor),
        subject: None,
        document: None,
        node: Some(NodeId(record.id)),
        detail: json!({ "kind": record.kind, "name": record.name, "parent": record.parent }),
    }
}

/// The event's document: a file's node is its document.
fn document_of(kind: NodeKind, id: NodeId) -> Option<DocId> {
    (kind == NodeKind::Document).then_some(DocId(id.0))
}

/// The word for `kind` in records and commit messages.
fn kind_word(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Site => "site",
        NodeKind::Library => "library",
        NodeKind::Folder => "folder",
        NodeKind::Document => "document",
    }
}

/// The node kind a record names.
fn parse_kind(word: &str) -> Option<NodeKind> {
    [
        NodeKind::Site,
        NodeKind::Library,
        NodeKind::Folder,
        NodeKind::Document,
    ]
    .into_iter()
    .find(|kind| kind_word(*kind) == word)
}

/// The commit author for `user`.
fn author_of(user: &User) -> Author {
    Author {
        id: user.id.0.to_string(),
        name: credited_name(&user.name),
    }
}

/// The commit author for changes the server makes on its own.
fn server_author() -> Author {
    Author {
        id: SERVER_AUTHOR_ID.to_owned(),
        name: SERVER_AUTHOR_NAME.to_owned(),
    }
}

/// Now, as a record's timestamp.
fn now_text() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// A record as PostgreSQL holds it.
fn to_row(record: &TreeRecord) -> Result<NodeRow, String> {
    let kind = parse_kind(&record.kind)
        .ok_or_else(|| format!("node {} has an unknown kind {:?}", record.id, record.kind))?;
    let time = |text: &str| {
        OffsetDateTime::parse(text, &Rfc3339)
            .map_err(|e| format!("node {} has an unreadable time {text:?}: {e}", record.id))
    };
    let user = |author: &Author| Uuid::parse_str(&author.id).ok().map(UserId);
    Ok(NodeRow {
        id: NodeId(record.id),
        kind,
        parent: record.parent.map(NodeId),
        site: NodeId(record.site),
        name: record.name.clone(),
        deleted_at: record.deleted.as_ref().map(|d| time(&d.at)).transpose()?,
        deleted_by: record.deleted.as_ref().and_then(|d| user(&d.by)),
        created_by: user(&record.created.by),
        created_at: time(&record.created.at)?,
        deleted_with: record.deleted_with.map(NodeId),
        bin_stage: record.bin_stage.map(i16::from),
        purged_at: record.purged.as_ref().map(|p| time(&p.at)).transpose()?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_and_made_unique() {
        assert_eq!(clean_name("x:y.docx", Editor::WordCraft), "x_y.docx");
        assert_eq!(clean_name("a?b*c", Editor::Text), "a_b_c");
        assert_eq!(clean_name("notes.", Editor::Text), "notes_");
        assert_eq!(clean_name("  ", Editor::Text), "Untitled");
        assert_eq!(clean_name(".xlsx", Editor::GridCraft), "Untitled.xlsx");
        let long = "a".repeat(200) + ".xlsx";
        assert_eq!(
            clean_name(&long, Editor::GridCraft).chars().count(),
            MAX_NAME_CHARS
        );
        let taken: HashSet<String> = ["a.xlsx".to_owned(), "a (2).xlsx".to_owned()].into();
        assert_eq!(
            unique_name("A.xlsx", Editor::GridCraft, &taken),
            "A (3).xlsx"
        );
        assert_eq!(unique_name("b.xlsx", Editor::GridCraft, &taken), "b.xlsx");
        let taken: HashSet<String> = ["m.solidcraft.json".to_owned()].into();
        assert_eq!(
            unique_name("M.solidcraft.json", Editor::SolidCraft, &taken),
            "M (2).solidcraft.json"
        );
        for name in ["x_y.docx", "a_b_c", "notes_", "Untitled.xlsx"] {
            assert!(validate_node_name(name).is_ok(), "{name}");
        }
    }
}
