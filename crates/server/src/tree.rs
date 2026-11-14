//! Changes to the content tree (create, rename, move, delete), the start-up reconcile of
//! PostgreSQL's mirror with Git, and adopting documents recorded before the tree.
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
    Access, AccessError, Db, DbError, Document, Editor, NewEvent, Node, NodeChange, NodeKind,
    NodeRow, Perms, User, validate_document_name, validate_node_name,
};
use revisions::{Author, CheckpointError, GitError, Stamp, TreeChange, TreeMove, TreeRecord};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::AppState;
use crate::checkpoint::Checkpointer;
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
    /// Deletes a file, or a folder with no undeleted children.
    Delete { id: NodeId },
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
/// `delete`); `InvalidName`, `WrongExtension` or `NameTaken` for a name;
/// `WrongPlace` for a kind that cannot go there or cannot be moved or deleted (and for files,
/// which [`create_document`] creates); `IntoItself` for a move into the node or below it;
/// `NotEmpty` for a folder with undeleted children; `IsSite` for deleting a site; `Exists` for
/// an identifier already used; `OutOfStep` while the tree needs repair; otherwise the Git or
/// database error.
pub async fn apply(state: &AppState, actor: &User, op: TreeOp) -> Result<Node, AccessError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let id = match &op {
        TreeOp::Create { id, .. }
        | TreeOp::Rename { id, .. }
        | TreeOp::Move { id, .. }
        | TreeOp::Delete { id } => *id,
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
    };
    if let Some(plan) = plan {
        commit(state, actor, plan).await?;
    }
    db.node(id).await?.ok_or(AccessError::NotFound)
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
    };
    commit(state, actor, plan).await?;
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
/// differs (kind, parent, site, name, or whether it is deleted) is written, parents before
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
async fn commit(state: &AppState, actor: &User, plan: Plan) -> Result<(), AccessError> {
    let store = state.checkpointer();
    let rows = plan
        .records
        .iter()
        .map(to_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbError::new)?;
    write_tree(
        store,
        author_of(actor),
        plan.message.clone(),
        plan.records.clone(),
    )
    .await
    .map_err(git_failure)?;
    let applied = state
        .db()
        .apply_nodes(NodeChange {
            rows: &rows,
            events: plan.events,
            actor: Some(actor.id),
            needs_admin: plan.needs_admin,
            needs: &plan.needs,
            site_groups: plan.site_groups.then_some(actor.id),
            document: plan.document,
        })
        .await;
    match applied {
        Ok(()) => Ok(()),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: tree change to node {} is in Git but not in the database: \
                 {error}",
                plan.id.0
            );
            let taken_back =
                plan.document.is_none() || take_back(state, actor, &plan.message, plan.undo).await;
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
            if !take_back(state, actor, &plan.message, plan.undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// Writes `undo` as a second commit, taking back the change `message` describes. Returns
/// whether it was written; if not, it is kept to be written before the next reconcile.
async fn take_back(state: &AppState, actor: &User, message: &str, undo: Vec<TreeRecord>) -> bool {
    let pending = PendingUndo {
        author: author_of(actor),
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
    let row = to_row(record)?;
    let kind = match current {
        None => "node.created",
        Some(now) if same(now, &row) => return Ok(false),
        Some(now) if now.deleted_at.is_some() && row.deleted_at.is_none() => "node.created",
        Some(now) if now.deleted_at.is_none() && row.deleted_at.is_some() => "node.deleted",
        Some(now) if now.parent != row.parent || now.site != row.site => "node.moved",
        Some(_) => "node.renamed",
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

/// Whether PostgreSQL's row says what Git's does.
fn same(now: &NodeRow, git: &NodeRow) -> bool {
    now.kind == git.kind
        && now.parent == git.parent
        && now.site == git.site
        && now.name == git.name
        && now.deleted_at.is_some() == git.deleted_at.is_some()
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
    }))
}

async fn plan_delete(state: &AppState, actor: &User, id: NodeId) -> Result<Plan, AccessError> {
    let db = state.db();
    let (node, access) = visible(db, actor, id).await?;
    match node.kind {
        NodeKind::Site => return Err(AccessError::IsSite),
        NodeKind::Library => return Err(AccessError::WrongPlace),
        NodeKind::Folder | NodeKind::Document => {}
    }
    require(&access, Perms::DELETE)?;
    if node.kind == NodeKind::Folder && db.has_live_children(id).await? {
        return Err(AccessError::NotEmpty);
    }
    let before = git_record(state, id).await?;
    let after = deleted(&before, author_of(actor));
    Ok(Plan {
        id,
        message: format!("Delete {} {}", kind_word(node.kind), node.name),
        events: vec![NewEvent {
            kind: "node.deleted",
            actor: Some(actor.id),
            subject: None,
            document: document_of(node.kind, id),
            node: Some(id),
            detail: json!({ "name": node.name }),
        }],
        records: vec![after],
        undo: vec![before],
        needs_admin: false,
        needs: vec![(id, Perms::DELETE)],
        site_groups: false,
        document: None,
    })
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
    }
}

/// `record`, deleted now by `by`.
fn deleted(record: &TreeRecord, by: Author) -> TreeRecord {
    let mut gone = record.clone();
    gone.deleted = Some(Stamp { at: now_text(), by });
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
