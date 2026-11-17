//! The content tree: sites, libraries, folders and files, mirrored from Git into PostgreSQL.

use collab_core::ids::{DocId, GroupId, NodeId, UserId};
use revisions::{BlobRef, parse_hex};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::documents::UNIQUE_VIOLATION;
use crate::events::{NewEvent, record, record_with_id};
use crate::groups::{
    add_site_owner, all_locked, ensure_site_groups, groups_judged_by, lock_groups,
};
use crate::libraries::{LibrarySettings, ensure_library_row, lock_library_rows, upsert_library};
use crate::links::{after_deletion, lock_links_below, revoke_locked};
use crate::quotas::{UsageChanges, apply_usage_changes, usage_changes};
use crate::roles::lock_roles;
use crate::scopes::{
    LOCK_ATTEMPTS, access_with, judged_roles, lock_segment_with, lock_top_down, scopes_of, segment,
};
use crate::sites::{SiteSettings, ensure_site_row, purge_site_rows, upsert_site};
use crate::{AccessError, Db, DbError, Editor, Perms};

const MAX_NAME_CHARS: usize = 120;
/// The events of a purge, whose detail gets what the purge gave back.
const PURGE_EVENTS: [&str; 2] = ["node.purged", "site.purged"];
const REFUSED_CHARS: [char; 9] = ['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// What a node of the tree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Site,
    Library,
    Folder,
    Document,
}

impl NodeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            NodeKind::Site => "site",
            NodeKind::Library => "library",
            NodeKind::Folder => "folder",
            NodeKind::Document => "document",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<NodeKind> {
        match text {
            "site" => Some(NodeKind::Site),
            "library" => Some(NodeKind::Library),
            "folder" => Some(NodeKind::Folder),
            "document" => Some(NodeKind::Document),
            _ => None,
        }
    }
}

/// A node as PostgreSQL holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// `None` exactly for a site.
    pub parent: Option<NodeId>,
    /// The site the node is in; a site's own id for the site itself.
    pub site: NodeId,
    pub name: String,
    /// Whether the node is a permission scope: it has its own assignments rather than
    /// inheriting. Sites always are.
    pub unique_scope: bool,
    pub deleted: bool,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
}

/// A node as Git holds it, which the projection writes into PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub id: NodeId,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub site: NodeId,
    pub name: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub deleted_by: Option<UserId>,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
    /// For a deleted node, the item whose deletion took it to the recycle bin: itself for the
    /// item a person deleted, that item for everything deleted with it. `None` on a deleted
    /// row is read as the node's own id (it was deleted before the bin).
    pub deleted_with: Option<NodeId>,
    /// For the item a person deleted, the bin stage it is in, 1 or 2. `None` on such an item
    /// that is not purged is read as 1.
    pub bin_stage: Option<i16>,
    /// When it was purged from the bin, if it was: the row stays as a tombstone.
    pub purged_at: Option<OffsetDateTime>,
}

impl NodeRow {
    /// The row with the recycle bin state it is stored with: a deleted row written before the
    /// bin is its own entry, in the first stage; only an entry that is not purged has a stage;
    /// a live row has no bin state. A deleted site is in no bin, so it has no stage either.
    #[must_use]
    pub fn normalized(&self) -> NodeRow {
        let mut row = self.clone();
        if row.deleted_at.is_none() {
            row.deleted_with = None;
            row.bin_stage = None;
            row.purged_at = None;
            return row;
        }
        let top = row.deleted_with.is_none_or(|with| with == row.id);
        if row.deleted_with.is_none() {
            row.deleted_with = Some(row.id);
        }
        if !top || row.purged_at.is_some() || row.kind == NodeKind::Site {
            row.bin_stage = None;
        } else if row.bin_stage.is_none() {
            row.bin_stage = Some(1);
        }
        row
    }
}

/// Whether `c` is an invisible format character (Unicode general category `Cf`): the
/// direction controls (U+202A to U+202E, U+2066 to U+2069, U+200E, U+200F, U+061C), the
/// zero-width characters (U+200B to U+200D, U+2060 to U+2064, U+FEFF), the soft hyphen, tag
/// characters and the like. In a name they can make one extension look like another
/// (`Invoice` + U+202E + `fdp.exe` shows as `Invoiceexe.pdf`), so names never hold them.
#[must_use]
pub fn is_format_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// Checks a name for any node: 1 to 120 characters after trimming, no control characters, no
/// invisible format characters (see [`is_format_char`]), none of `" * : < > ? / \ |`, and no
/// trailing dot. Returns the trimmed name.
pub fn validate_node_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let length = name.chars().count();
    let well_formed = (1..=MAX_NAME_CHARS).contains(&length)
        && !name.ends_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || is_format_char(c) || REFUSED_CHARS.contains(&c));
    if well_formed {
        Ok(name)
    } else {
        Err(AccessError::InvalidName)
    }
}

pub(crate) fn node_from_row(row: &PgRow) -> Result<Node, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    Ok(Node {
        id: NodeId(row.try_get("id")?),
        kind: NodeKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown node kind {kind:?}").into()))?,
        parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
        site: NodeId(row.try_get("site_id")?),
        name: row.try_get("name")?,
        unique_scope: row.try_get("unique_scope")?,
        deleted: row.try_get("deleted")?,
        created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
        created_at: row.try_get("created_at")?,
    })
}

/// A change to the tree as PostgreSQL applies it once Git has recorded it: the rows, their
/// events, and what the person making it must still be allowed to do, judged in the same
/// transaction with the rows concerned locked.
#[derive(Clone, Debug, Default)]
pub struct NodeChange<'a> {
    /// The nodes as Git now holds them, each parent before its children.
    pub rows: &'a [NodeRow],
    /// The events to record with the change.
    pub events: Vec<NewEvent>,
    /// Who makes the change; `None` for the server itself.
    pub actor: Option<UserId>,
    /// Whether `actor` must be an enabled administrator.
    pub needs_admin: bool,
    /// Nodes `actor` must hold the given permissions on. Each node's chain, from the scope
    /// that decides its access down to it, is locked in this order (the node itself
    /// `FOR UPDATE` if it is one of `rows`, everything else `FOR SHARE`), then the groups and
    /// the roles giving the access, before it is judged: list ancestors before descendants, as
    /// access changes lock them.
    pub needs: &'a [(NodeId, Perms)],
    /// The first row's node is a new site: makes this person (its creator) a member of its
    /// Owners group. Every site row gets its three groups, with their assignments, in the same
    /// transaction, whether or not this is given.
    pub site_groups: Option<UserId>,
    /// Records the first row's node as a new document opened in this editor, created by
    /// `actor`.
    pub document: Option<Editor>,
    /// Records each of these nodes, among `rows`, as a new document opened in its editor,
    /// created by `actor`: the files of a copy.
    pub copies: &'a [(NodeId, Editor)],
    /// How many of the last `needs` are places below a folder being deleted: a refusal on one of
    /// them is `ForbiddenBelow`.
    pub needs_below: usize,
    /// Judges `needs` as if the nodes were live (see [`Db::access_as_if_live`]): for restoring
    /// an item from the recycle bin, or taking it further out.
    pub as_if_live: bool,
    /// Sites whose `sites` row the change writes: locked `FOR UPDATE`, by id, after the node
    /// rows and before the links, usage rows, groups and roles.
    pub sites: &'a [NodeId],
    /// The `sites` rows of new sites among `rows`, written with exactly these values (rather
    /// than the organisation's defaults as they are now), after the rows.
    pub new_sites: &'a [SiteSettings],
    /// The ids the first row's new site gives its Owners, Members and Visitors groups, in that
    /// order: chosen before the change so its navigation can aim at them. New random ids when
    /// `None`.
    pub site_group_ids: Option<[Uuid; 3]>,
    /// Libraries whose `libraries` row the change writes: locked `FOR UPDATE`, by id, after
    /// the `sites` rows and before the links, usage rows, groups and roles.
    pub libraries: &'a [NodeId],
    /// The `libraries` rows of new libraries among `rows`, written with exactly these values
    /// (rather than the defaults), after the rows.
    pub new_libraries: &'a [LibrarySettings],
    /// The deleted site whose purge this is (every row of it among `rows`, purged): once the
    /// rows are written, everything else the site owned goes in the same transaction (see
    /// [`crate::sites::purge_site_rows`]).
    pub purge_site: Option<NodeId>,
}

/// Inserts or updates `row`, and for a document node the name of its `documents` row. A site
/// gets the groups it does not have yet (see [`ensure_site_groups`]).
pub(crate) async fn upsert_node(conn: &mut PgConnection, row: &NodeRow) -> Result<(), AccessError> {
    upsert_node_with(conn, row, None).await
}

/// [`upsert_node`], a site's missing groups made with the ids `group_ids` when given.
async fn upsert_node_with(
    conn: &mut PgConnection,
    row: &NodeRow,
    group_ids: Option<[Uuid; 3]>,
) -> Result<(), AccessError> {
    let row = &row.normalized();
    let result = sqlx::query(
        "insert into nodes \
             (id, kind, parent_id, site_id, name, unique_scope, deleted_at, deleted_by, \
              created_by, created_at, deleted_with, bin_stage, purged_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         on conflict (id) do update set \
             kind = excluded.kind, parent_id = excluded.parent_id, \
             site_id = excluded.site_id, name = excluded.name, \
             deleted_at = excluded.deleted_at, deleted_by = excluded.deleted_by, \
             deleted_with = excluded.deleted_with, bin_stage = excluded.bin_stage, \
             purged_at = excluded.purged_at",
    )
    .bind(row.id.0)
    .bind(row.kind.as_str())
    .bind(row.parent.map(|p| p.0))
    .bind(row.site.0)
    .bind(&row.name)
    .bind(row.kind == NodeKind::Site)
    .bind(row.deleted_at)
    .bind(row.deleted_by.map(|u| u.0))
    .bind(row.created_by.map(|u| u.0))
    .bind(row.created_at)
    .bind(row.deleted_with.map(|n| n.0))
    .bind(row.bin_stage)
    .bind(row.purged_at)
    .execute(&mut *conn)
    .await;
    match result {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
            return Err(AccessError::NameTaken);
        }
        Err(e) => return Err(e.into()),
    }
    // A purged site keeps no groups and no settings of its own.
    if row.kind == NodeKind::Site && row.purged_at.is_none() {
        ensure_site_groups(&mut *conn, row.id, group_ids).await?;
        ensure_site_row(&mut *conn, row.id).await?;
    }
    if row.kind == NodeKind::Library && row.purged_at.is_none() {
        ensure_library_row(&mut *conn, row.id).await?;
    }
    if row.kind == NodeKind::Document {
        sqlx::query("update documents set name = $2 where id = $1")
            .bind(row.id.0)
            .bind(&row.name)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// The live links a tree change revokes because it deletes their item or a node above it, each
/// with its group: locked by [`judge`], revoked once the rows are written.
type Doomed = Vec<(uuid::Uuid, Option<GroupId>)>;

/// What [`judge`] locked for a tree change: the live links it revokes (see [`Doomed`]) and the
/// links, revoked or not, of the nodes it purges, which go with them.
#[derive(Default)]
pub(crate) struct Locked {
    doomed: Doomed,
    purged_links: Doomed,
    /// What the change does to sites' usage, their rows locked.
    usage: UsageChanges,
}

/// The nodes `change` marks deleted and does not purge, and the sites they are in. A site's own
/// deletion is left out: it revokes nothing and closes nothing, so that restoring the site
/// gives everything back as it was.
fn deleting(change: &NodeChange<'_>) -> (Vec<NodeId>, Vec<NodeId>) {
    let rows = change.rows.iter().filter(|row| {
        row.deleted_at.is_some() && row.purged_at.is_none() && row.kind != NodeKind::Site
    });
    let nodes = rows.clone().map(|row| row.id).collect();
    let mut sites: Vec<NodeId> = rows.map(|row| row.site).collect();
    sites.sort_unstable();
    sites.dedup();
    (nodes, sites)
}

/// The nodes `change` purges.
fn purging(change: &NodeChange<'_>) -> Vec<NodeId> {
    change
        .rows
        .iter()
        .filter(|row| row.purged_at.is_some())
        .map(|row| row.id)
        .collect()
}

/// Locks, `FOR UPDATE` and by id, every link of the nodes `purged`, revoked or not, and returns
/// them with their groups.
async fn lock_links_of(conn: &mut PgConnection, purged: &[NodeId]) -> Result<Doomed, sqlx::Error> {
    if purged.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = purged.iter().map(|id| id.0).collect();
    let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "select id, group_id from links where node_id = any($1) order by id for update",
    )
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, group)| (id, group.map(GroupId)))
        .collect())
}

/// Locks the `sites` rows of `sites`, by id, `FOR UPDATE`.
async fn lock_site_rows(conn: &mut PgConnection, sites: &[NodeId]) -> Result<(), sqlx::Error> {
    if sites.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<Uuid> = sites.iter().map(|id| id.0).collect();
    ids.sort_unstable();
    sqlx::query("select id from sites where id = any($1) order by id for update")
        .bind(&ids)
        .execute(conn)
        .await?;
    Ok(())
}

/// What purging removed of the documents' versions: how many rows named a content, the sum of
/// those contents' sizes, and the distinct contents they named.
#[derive(Clone, Debug, Default)]
struct PurgedVersions {
    versions: i64,
    bytes: i64,
    contents: Vec<BlobRef>,
}

/// What [`Db::apply_nodes`] did besides writing the rows, for the caller to finish once it has
/// committed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodesApplied {
    /// The contents the versions of the documents it purged named, and those a purged site's
    /// look pinned; nothing in `blob_refs` or `brand_refs` names them for those documents or
    /// that site any more, so each is erased if nothing else does.
    pub released: Vec<BlobRef>,
    /// The ids of the `node.purged` and `site.purged` events it recorded.
    pub purge_events: Vec<i64>,
}

/// The rest of purging the nodes `purged`, once their rows are written: their assignments go,
/// their links (locked by [`lock_links_of`]) are revoked if still live and then deleted with
/// their hidden groups, the access requests still open on them are closed, and the rows naming
/// their documents' contents in `blob_refs` are deleted (erasing what nothing names any more is
/// the caller's).
async fn after_purge(
    conn: &mut PgConnection,
    actor: Option<UserId>,
    purged: &[NodeId],
    links: &Doomed,
) -> Result<PurgedVersions, sqlx::Error> {
    let ids: Vec<Uuid> = purged.iter().map(|id| id.0).collect();
    for (link, group) in links {
        revoke_locked(&mut *conn, actor, *link, "item purged").await?;
        sqlx::query("delete from links where id = $1")
            .bind(link)
            .execute(&mut *conn)
            .await?;
        if let Some(group) = group {
            sqlx::query("delete from group_members where group_id = $1")
                .bind(group.0)
                .execute(&mut *conn)
                .await?;
            sqlx::query(
                "delete from assignments where principal_kind = 'group' and principal_id = $1",
            )
            .bind(group.0)
            .execute(&mut *conn)
            .await?;
            sqlx::query("delete from groups where id = $1 and purpose = 'link'")
                .bind(group.0)
                .execute(&mut *conn)
                .await?;
        }
    }
    sqlx::query("delete from assignments where node_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    // A purged node of a connected library no longer holds its repository path; the next sync
    // that sees the path adds it afresh.
    sqlx::query("delete from source_items where node_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "update access_requests set decided_at = now(), decision = 'closed' \
         where decided_at is null and node_id = any($1)",
    )
    .bind(&ids)
    .execute(&mut *conn)
    .await?;
    let gone: Vec<(String, i64)> = sqlx::query_as(
        "with gone as (delete from blob_refs where doc_id = any($1) returning sha256) \
         select b.sha256, b.size from gone join blobs b on b.sha256 = gone.sha256",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut purged = PurgedVersions::default();
    let mut seen = std::collections::HashSet::new();
    for (sha256, size) in gone {
        purged.versions += 1;
        purged.bytes = purged.bytes.saturating_add(size);
        if seen.insert(sha256.clone())
            && let (Some(sha256), Ok(size)) = (parse_hex(&sha256), u64::try_from(size))
        {
            purged.contents.push(BlobRef { sha256, size });
        }
    }
    Ok(purged)
}

/// Locks what `change` needs and judges its actor's access. Each needed node's chain is locked
/// from its source scope down, as access changes lock it (ancestors `FOR SHARE`, a node the
/// change writes `FOR UPDATE`), in `needs` order; then every row the change writes,
/// `FOR UPDATE`, ancestors first (see `lock_top_down`); then the live links at or below a node
/// it deletes, by id, `FOR UPDATE`; then the usage rows of the sites whose usage it changes
/// (a purge, a move to another site), by site id, `FOR UPDATE`; then the groups the actor is a
/// member of that
/// are assigned on those chains' scopes, by id, `FOR SHARE`, with those links' groups
/// `FOR UPDATE`; then the roles giving that access,
/// by id, `FOR SHARE`; then the access is worked out again and judged. So an assignment changed
/// anywhere above a needed node, a membership ended of a group giving the access, or a role
/// changed that gives it, either waits for this change or is seen by it. Returns the links to
/// revoke, or `None` if a chain or the groups moved while they were being locked: the caller
/// starts over. The links of nodes it purges are locked with the live links, after them, and
/// their groups `FOR UPDATE` with those links' groups.
async fn judge(
    conn: &mut PgConnection,
    change: &NodeChange<'_>,
) -> Result<Option<Locked>, AccessError> {
    // The rows written are locked in the one order for several nodes (see `lock_top_down`)
    // before any is written, whatever order they come in (a move's subtree is read parents
    // first, otherwise in any order), and before any group or role: every transaction takes
    // all the node rows it needs before a group, so none can hold a group while waiting for a
    // node that another holds while waiting for that group.
    let written: Vec<NodeId> = change.rows.iter().map(|row| row.id).collect();
    let (deleted, sites) = deleting(change);
    let purged = purging(change);
    if !change.needs_admin && change.needs.is_empty() {
        lock_top_down(&mut *conn, &written, true).await?;
        lock_site_rows(&mut *conn, change.sites).await?;
        lock_library_rows(&mut *conn, change.libraries).await?;
        let doomed = lock_links_below(&mut *conn, &deleted, &sites).await?;
        let purged_links = lock_links_of(&mut *conn, &purged).await?;
        let usage = usage_changes(&mut *conn, change.rows).await?;
        lock_groups(
            &mut *conn,
            [],
            doomed.iter().chain(&purged_links).filter_map(|(_, g)| *g),
        )
        .await?;
        return Ok(Some(Locked {
            doomed,
            purged_links,
            usage,
        }));
    }
    let actor = change.actor.ok_or(AccessError::Forbidden)?;
    if change.needs_admin {
        let admin: Option<bool> = sqlx::query_scalar(
            "select is_admin and disabled_at is null from users where id = $1 for share",
        )
        .bind(actor.0)
        .fetch_optional(&mut *conn)
        .await?;
        if admin != Some(true) {
            return Err(AccessError::Forbidden);
        }
    }
    let mut involved = Vec::new();
    let mut scopes = Vec::new();
    for (id, _) in change.needs {
        let written = change.rows.iter().any(|row| row.id == *id);
        let Some(links) =
            lock_segment_with(&mut *conn, actor, *id, false, written, change.as_if_live).await?
        else {
            return Ok(None);
        };
        scopes.extend(scopes_of(&segment(&links, false)));
        involved.extend(judged_roles(&links));
    }
    lock_top_down(&mut *conn, &written, true).await?;
    lock_site_rows(&mut *conn, change.sites).await?;
    lock_library_rows(&mut *conn, change.libraries).await?;
    let doomed = lock_links_below(&mut *conn, &deleted, &sites).await?;
    let purged_links = lock_links_of(&mut *conn, &purged).await?;
    let usage = usage_changes(&mut *conn, change.rows).await?;
    let judged = groups_judged_by(&mut *conn, actor, &scopes).await?;
    lock_groups(
        &mut *conn,
        judged.iter().copied(),
        doomed.iter().chain(&purged_links).filter_map(|(_, g)| *g),
    )
    .await?;
    lock_roles(&mut *conn, involved).await?;
    if !all_locked(
        &groups_judged_by(&mut *conn, actor, &scopes).await?,
        &judged,
    ) {
        return Ok(None);
    }
    // Every chain and role is locked now, so none of this access can change before the commit.
    let below_from = change.needs.len().saturating_sub(change.needs_below);
    for (at, (id, needed)) in change.needs.iter().enumerate() {
        let access = access_with(&mut *conn, actor, *id, change.as_if_live).await?;
        let allowed = match &access {
            None if at < below_from => return Err(AccessError::NotFound),
            None => false,
            Some(access) => access.perms.contains(*needed),
        };
        if !allowed {
            // A refusal because the site is archived says so, wherever it is judged.
            return Err(match access {
                Some(access) if access.archived() => AccessError::Archived,
                _ if at < below_from => AccessError::Forbidden,
                _ => AccessError::ForbiddenBelow,
            });
        }
    }
    Ok(Some(Locked {
        doomed,
        purged_links,
        usage,
    }))
}

impl Db {
    /// Applies one node as Git holds it, in one transaction: inserts or updates the row (kind,
    /// parent, site, name, deleted), writes `event` if given, and for a document node updates
    /// the name of the `documents` row with that id, if there is one, so rooms, downloads and
    /// saves use the new name. A new site row is a scope; an update never changes
    /// `unique_scope`. A name already used by an undeleted sibling gives `NameTaken`.
    pub async fn apply_node(
        &self,
        row: &NodeRow,
        event: Option<NewEvent>,
    ) -> Result<(), AccessError> {
        self.apply_nodes(NodeChange {
            rows: std::slice::from_ref(row),
            events: event.into_iter().collect(),
            ..NodeChange::default()
        })
        .await
        .map(drop)
    }

    /// Applies `change` in one transaction: locks and judges what it needs (`NotFound` if the
    /// actor no longer has access to a node it needs, `Forbidden` if it lacks a permission or
    /// they are no longer an enabled administrator when that is needed), records the new
    /// document if any (`Exists` if its id is taken), applies each row as
    /// [`Db::apply_node`] does (a site row gets its three groups), makes the new site's creator a
    /// member of its Owners group if asked, and writes the events. A node it marks deleted has its
    /// live links, and those of every node below it, revoked (reason `item deleted`), and the
    /// open access requests on it and below it closed, in the same transaction. A node it marks
    /// purged loses its assignments, its links with their hidden groups, its open access
    /// requests and the rows naming its versions' contents, in the same transaction; each
    /// `node.purged` event's detail gets how many versions went (`versions`) and the sum of
    /// their contents' sizes (`bytes`). Answers the contents those versions named and the ids
    /// of the `node.purged` events, for the caller to erase what nothing names any more once
    /// this has committed. A document a copy makes (`copies`) is recorded as `document`'s is.
    ///
    /// A database failure before the change was judged allowed (the connection lost while
    /// locking, or the tree changing on every attempt) is `Unjudged`: nothing was written, and
    /// the change must be treated as refused. `Db` is a statement failure after it was judged
    /// allowed; `CommitOutcomeUnknown` means COMMIT failed, so whether it took effect is unknown.
    pub async fn apply_nodes(&self, change: NodeChange<'_>) -> Result<NodesApplied, AccessError> {
        let (mut tx, locked) = self.judged(&change).await.map_err(|e| match e {
            AccessError::Db(e) => AccessError::Unjudged(e),
            other => other,
        })?;
        let first = change
            .document
            .zip(change.rows.first())
            .map(|(editor, row)| (row.id, editor));
        for (id, editor) in first.iter().chain(change.copies) {
            let Some(row) = change.rows.iter().find(|row| row.id == *id) else {
                return Err(DbError::new("a new document has no node among the rows").into());
            };
            let inserted = sqlx::query(
                "insert into documents (id, name, editor, created_by) values ($1, $2, $3, $4)",
            )
            .bind(row.id.0)
            .bind(&row.name)
            .bind(editor.id())
            .bind(change.actor.map(|u| u.0))
            .execute(&mut *tx)
            .await;
            match inserted {
                Ok(_) => {}
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                    return Err(AccessError::Exists);
                }
                Err(e) => return Err(e.into()),
            }
        }
        // `judge` has locked every row written, in the one order, before any group or role.
        let first = change.rows.first().map(|row| row.id);
        for row in change.rows {
            let ids = change.site_group_ids.filter(|_| Some(row.id) == first);
            upsert_node_with(&mut tx, row, ids).await?;
        }
        for site in change.new_sites {
            upsert_site(&mut tx, site, change.actor.is_none()).await?;
        }
        for library in change.new_libraries {
            upsert_library(&mut tx, library).await?;
        }
        // A person's move into a site without room is refused; Git's own changes are mirrored.
        apply_usage_changes(&mut tx, &locked.usage, change.actor.is_some()).await?;
        if let (Some(creator), Some(first)) = (change.site_groups, change.rows.first()) {
            add_site_owner(&mut tx, first.id, creator, change.actor).await?;
        }
        let (deleted, sites) = deleting(&change);
        if !deleted.is_empty() {
            after_deletion(&mut tx, change.actor, &locked.doomed, &sites).await?;
        }
        let purged = purging(&change);
        let mut events = change.events;
        let mut done = NodesApplied::default();
        if !purged.is_empty() {
            let gone = after_purge(&mut tx, change.actor, &purged, &locked.purged_links).await?;
            for event in events.iter_mut().filter(|e| PURGE_EVENTS.contains(&e.kind)) {
                if let Some(detail) = event.detail.as_object_mut() {
                    detail.insert("versions".to_owned(), gone.versions.into());
                    detail.insert("bytes".to_owned(), gone.bytes.into());
                }
            }
            done.released = gone.contents;
        }
        let purged_documents: Vec<DocId> = change
            .rows
            .iter()
            .filter(|row| row.kind == NodeKind::Document && row.purged_at.is_some())
            .map(|row| DocId(row.id.0))
            .collect();
        crate::live::delete_live_logs(&mut tx, &purged_documents).await?;
        if let Some(site) = change.purge_site {
            for blob in purge_site_rows(&mut tx, change.actor, site).await? {
                if !done.released.iter().any(|b| b.sha256 == blob.sha256) {
                    done.released.push(blob);
                }
            }
        }
        for event in events {
            if PURGE_EVENTS.contains(&event.kind) {
                done.purge_events
                    .push(record_with_id(&mut tx, event).await?);
            } else {
                record(&mut tx, event).await?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| AccessError::CommitOutcomeUnknown(e.into()))?;
        Ok(done)
    }

    /// A transaction in which `change` has been judged allowed, with what it needs locked, and
    /// the links it revokes or deletes.
    pub(crate) async fn judged(
        &self,
        change: &NodeChange<'_>,
    ) -> Result<(sqlx::Transaction<'static, sqlx::Postgres>, Locked), AccessError> {
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            if let Some(locked) = judge(&mut tx, change).await? {
                return Ok((tx, locked));
            }
            tx.rollback().await?;
        }
        Err(DbError::new("the tree kept changing while a change was judged").into())
    }

    /// Every node as PostgreSQL holds it, deleted or not, for comparing with Git.
    pub async fn node_rows(&self) -> Result<Vec<NodeRow>, DbError> {
        let rows = sqlx::query(
            "select id, kind, parent_id, site_id, name, deleted_at, deleted_by, created_by, \
                    created_at, deleted_with, bin_stage, purged_at from nodes",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                Ok(NodeRow {
                    id: NodeId(row.try_get("id")?),
                    kind: NodeKind::parse(&kind).ok_or_else(|| {
                        sqlx::Error::Decode(format!("unknown node kind {kind:?}").into())
                    })?,
                    parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
                    site: NodeId(row.try_get("site_id")?),
                    name: row.try_get("name")?,
                    deleted_at: row.try_get("deleted_at")?,
                    deleted_by: row.try_get::<Option<Uuid>, _>("deleted_by")?.map(UserId),
                    created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
                    created_at: row.try_get("created_at")?,
                    deleted_with: row.try_get::<Option<Uuid>, _>("deleted_with")?.map(NodeId),
                    bin_stage: row.try_get("bin_stage")?,
                    purged_at: row.try_get("purged_at")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Whether no undeleted site other than `except` is named `name`, ignoring case.
    pub async fn site_name_free(
        &self,
        name: &str,
        except: Option<NodeId>,
    ) -> Result<bool, DbError> {
        let taken: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes \
                 where parent_id is null and lower(name) = lower($1) and deleted_at is null \
                   and ($2::uuid is null or id <> $2))",
        )
        .bind(name)
        .bind(except.map(|e| e.0))
        .fetch_one(self.pool())
        .await?;
        Ok(!taken)
    }

    /// The node with this id, deleted or not, whoever may see it.
    pub async fn node(&self, id: NodeId) -> Result<Option<Node>, DbError> {
        let row = sqlx::query(
            "select id, kind, parent_id, site_id, name, unique_scope, \
                    (deleted_at is not null) as deleted, created_by, created_at \
             from nodes where id = $1",
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(node_from_row).transpose()?)
    }

    /// Whether `id` has ever had a direct child, including one now in the recycle bin.
    pub async fn has_any_children(&self, id: NodeId) -> Result<bool, DbError> {
        Ok(
            sqlx::query_scalar("select exists (select 1 from nodes where parent_id = $1)")
                .bind(id.0)
                .fetch_one(self.pool())
                .await?,
        )
    }

    /// Whether `node` is `ancestor` or lies anywhere below it. False if `node` does not exist.
    /// The walk ends even if the data held a cycle.
    pub async fn is_at_or_below(&self, node: NodeId, ancestor: NodeId) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(
            "with recursive up(id, parent_id) as ( \
                 select id, parent_id from nodes where id = $1 \
                 union \
                 select n.id, n.parent_id from nodes n join up on n.id = up.parent_id \
             ) select exists (select 1 from up where id = $2)",
        )
        .bind(node.0)
        .bind(ancestor.0)
        .fetch_one(self.pool())
        .await?;
        Ok(found)
    }

    /// The live nodes strictly below `node` with permissions of their own, ancestors before
    /// descendants: what deleting `node` must also be allowed on. Read from the site's scopes,
    /// so its cost follows how many places have their own permissions, not how many items.
    pub async fn scopes_below(&self, node: NodeId) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "with recursive up(origin, id, parent_id, depth) as (                  select s.id, s.id, s.parent_id, 0 from nodes s                  where s.site_id = (select site_id from nodes where id = $1)                    and s.unique_scope and s.deleted_at is null and s.id <> $1                  union all                  select up.origin, n.id, n.parent_id, up.depth + 1                  from up join nodes n on n.id = up.parent_id where up.depth < 10000)              select origin from up group by origin              having bool_or(id = $1) order by max(depth), origin",
        )
        .bind(node.0)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Whether `id` has any undeleted child.
    pub async fn has_live_children(&self, id: NodeId) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes where parent_id = $1 and deleted_at is null)",
        )
        .bind(id.0)
        .fetch_one(self.pool())
        .await?;
        Ok(found)
    }

    /// The names of `parent`'s undeleted children, lower-cased, for finding a free name.
    pub async fn live_child_names(&self, parent: NodeId) -> Result<Vec<String>, DbError> {
        let names = sqlx::query_scalar(
            "select lower(name) from nodes where parent_id = $1 and deleted_at is null",
        )
        .bind(parent.0)
        .fetch_all(self.pool())
        .await?;
        Ok(names)
    }

    /// The undeleted child of `parent` named `name`, ignoring case, if there is one: the item a
    /// new file of that name would replace.
    pub async fn live_child_named(
        &self,
        parent: NodeId,
        name: &str,
    ) -> Result<Option<Node>, DbError> {
        let row = sqlx::query(
            "select id, kind, parent_id, site_id, name, unique_scope, \
                    (deleted_at is not null) as deleted, created_by, created_at \
             from nodes \
             where parent_id = $1 and lower(name) = lower($2) and deleted_at is null",
        )
        .bind(parent.0)
        .bind(name)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(node_from_row).transpose()?)
    }

    /// Whether no undeleted child of `parent`, other than `except`, is named `name`, ignoring
    /// case.
    pub async fn name_free(
        &self,
        parent: NodeId,
        name: &str,
        except: Option<NodeId>,
    ) -> Result<bool, DbError> {
        let taken: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes \
                 where parent_id = $1 and lower(name) = lower($2) and deleted_at is null \
                   and ($3::uuid is null or id <> $3))",
        )
        .bind(parent.0)
        .bind(name)
        .bind(except.map(|e| e.0))
        .fetch_one(self.pool())
        .await?;
        Ok(!taken)
    }
}
