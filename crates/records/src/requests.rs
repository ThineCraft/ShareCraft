//! Requests for access. Someone who reaches an item they cannot open may ask for access; the
//! people who manage access to it see the request and approve it with a role, given directly
//! on the item through [`Db::set_node_access`]'s own path, or decline it with a short reply.
//!
//! A request never reveals whether its item exists: [`Db::request_access`] answers the same,
//! and does the same work, for an item that exists, one that does not, one that is deleted and
//! one the person can already open, and the daily limit counts every submission. A person sees
//! their own requests only once someone decided them, and the item's name only while they can
//! see it.
//!
//! Locking: a submission first takes a transaction-scoped advisory lock on its requester (never
//! the `users` row), then the item's chain `FOR SHARE` from the top down. Deciding follows the
//! one order (the chain, then groups, then roles) and takes the request's row last, as a tree
//! change closing requests on a deleted item does.

use collab_core::ids::{NodeId, UserId};
use serde_json::json;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::events::{NewEvent, record};
use crate::explorer::UserRef;
use crate::scopes::{
    MANAGE_ACCESS_BIT, MAX_DEPTH, MINE, Standing, assign_locked, assignment_on, chain, crumb,
    document_of, manager, path_in, source_index, standing,
};
use crate::{AccessError, Crumb, Db, DbError, Principal, RoleRef};

/// How many requests for access one person may submit in any 24 hours, recorded or not.
pub const REQUESTS_PER_DAY: i64 = 20;

/// The longest message or reply on a request, in characters.
pub const REQUEST_MESSAGE_MAX: usize = 500;

/// Why a request for access was not taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// The person has submitted [`REQUESTS_PER_DAY`] requests in the last 24 hours. Checked
    /// before anything about the item, so it says nothing about it.
    #[error("too many requests; try again later")]
    TooMany,
    /// The message is longer than [`REQUEST_MESSAGE_MAX`] characters. Checked before anything
    /// else.
    #[error("that message is too long")]
    MessageTooLong,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for RequestError {
    fn from(e: sqlx::Error) -> Self {
        RequestError::Db(e.into())
    }
}

/// An open request for access, as the people who manage its item see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessRequest {
    pub id: Uuid,
    /// The item asked for, with the manager's access to it.
    pub node: Crumb,
    /// The item's place: from its site down to its parent, with the manager's access to each.
    pub path: Vec<Crumb>,
    pub requester: UserRef,
    pub message: Option<String>,
    pub created_at: OffsetDateTime,
}

/// How a request was decided, as its requester sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestDecision {
    /// Approved with a role.
    Approved,
    Declined,
}

/// One of a person's own decided requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MyRequest {
    pub id: Uuid,
    /// The item, with the person's access to it now; `None` unless they can see it now (have
    /// access to it, or to something below it).
    pub node: Option<Crumb>,
    /// The item's place from its site down to its parent; empty when `node` is `None`.
    pub path: Vec<Crumb>,
    pub decision: RequestDecision,
    /// The role given, for an approved request whose role still exists.
    pub role: Option<RoleRef>,
    /// The manager's reply to a declined request, if they wrote one.
    pub reply: Option<String>,
    pub created_at: OffsetDateTime,
    pub decided_at: OffsetDateTime,
}

/// A message trimmed, `None` when empty; `None` (refused) when longer than allowed.
fn checked_message(message: Option<&str>) -> Option<Option<String>> {
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    match message {
        Some(m) if m.chars().count() > REQUEST_MESSAGE_MAX => None,
        other => Some(other.map(str::to_owned)),
    }
}

/// The open requests on live items that `$1` manages access to (every one for an
/// administrator, `$2`), from enabled requesters: the CTE `managed(id, node_id)`, after
/// `with recursive`.
fn managed_requests() -> String {
    format!(
        "{MINE}, \
         open(id, node_id) as ( \
             select r.id, r.node_id from access_requests r \
             join users u on u.id = r.requester and u.disabled_at is null \
             where r.decided_at is null), \
         up(origin, id, parent_id, scope, deleted, depth) as ( \
             select n.id, n.id, n.parent_id, n.unique_scope, n.deleted_at is not null, 0 \
             from nodes n where n.id in (select node_id from open) \
             union all \
             select up.origin, n.id, n.parent_id, n.unique_scope, n.deleted_at is not null, \
                    up.depth + 1 \
             from up join nodes n on n.id = up.parent_id where up.depth < {MAX_DEPTH}), \
         alive(origin) as (select origin from up group by origin having not bool_or(deleted)), \
         source(origin, id) as ( \
             select distinct on (origin) origin, id from up where scope \
             order by origin, depth), \
         managed(id, node_id) as ( \
             select o.id, o.node_id from open o \
             join source s on s.origin = o.node_id \
             where o.node_id in (select origin from alive) \
               and ($2 or exists (select 1 from mine m \
                                  where m.node_id = s.id \
                                    and m.perms & {MANAGE_ACCESS_BIT} <> 0)))"
    )
}

/// Whether `user` is an enabled administrator; `None` for a disabled or unknown account.
async fn admin_or_none(conn: &mut PgConnection, user: UserId) -> Result<Option<bool>, sqlx::Error> {
    Ok(match standing(conn, user).await? {
        Standing::Nobody => None,
        Standing::Member => Some(false),
        Standing::Admin => Some(true),
    })
}

impl Db {
    /// Asks for access to `node` for `user`, with an optional `message` (trimmed; at most
    /// [`REQUEST_MESSAGE_MAX`] characters, `MessageTooLong`). Always `Ok(())` when taken,
    /// whether or not the item exists: a request is recorded only for a live item the person
    /// cannot open, and the people who manage it see it; an unknown, deleted or
    /// already-accessible item records nothing. A second open request for the same item
    /// replaces the first one's message. Records `request.created` for a new request.
    ///
    /// The daily limit is checked first: under a transaction-scoped advisory lock on the
    /// person, their submissions of the last 24 hours are counted (`TooMany` at
    /// [`REQUESTS_PER_DAY`]) and this one is recorded, whatever follows. Only then is the item
    /// looked at, by the same statements whatever it is: its chain is locked `FOR SHARE` from
    /// the top down (so a deletion either closes the request or comes after it), the person's
    /// access is worked out, and the request is written only if it is wanted.
    pub async fn request_access(
        &self,
        user: UserId,
        node: NodeId,
        message: Option<&str>,
    ) -> Result<(), RequestError> {
        let message = checked_message(message).ok_or(RequestError::MessageTooLong)?;
        let mut tx = self.pool().begin().await?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('request:' || $1::text, 0))")
            .bind(user.0)
            .execute(&mut *tx)
            .await?;
        let recent: i64 = sqlx::query_scalar(
            "select count(*) from request_attempts \
             where requester = $1 and at > now() - interval '1 day'",
        )
        .bind(user.0)
        .fetch_one(&mut *tx)
        .await?;
        if recent >= REQUESTS_PER_DAY {
            tx.rollback().await?;
            return Err(RequestError::TooMany);
        }
        sqlx::query("insert into request_attempts (requester) values ($1)")
            .bind(user.0)
            .execute(&mut *tx)
            .await?;
        // The node and everything above it, deleted or not, locked from the top down in one
        // statement; nothing for an unknown node.
        let locked: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "with recursive up(id, parent_id, depth) as ( \
                 select id, parent_id, 0 from nodes where id = $1 \
                 union all \
                 select n.id, n.parent_id, up.depth + 1 from up \
                 join nodes n on n.id = up.parent_id where up.depth < {MAX_DEPTH}) \
             select n.id from nodes n join up on up.id = n.id \
             order by up.depth desc for share of n"
        )))
        .bind(node.0)
        .fetch_all(&mut *tx)
        .await?;
        let links = chain(&mut tx, user, node).await?;
        // Wanted only for a live item the person cannot open, whose chain is the one locked.
        let wanted = links.as_ref().is_some_and(|links| {
            links.last().is_some_and(|link| link.role.is_none())
                && links
                    .iter()
                    .map(|link| link.node.id.0)
                    .eq(locked.iter().copied())
        });
        let written: Option<(Uuid, bool)> = sqlx::query_as(
            "insert into access_requests (id, node_id, requester, message) \
             select $1, $2, $3, $4 where $5 \
             on conflict (node_id, requester) where decided_at is null \
             do update set message = excluded.message \
             returning id, (xmax = 0)",
        )
        .bind(Uuid::new_v4())
        .bind(node.0)
        .bind(user.0)
        .bind(message.as_deref())
        .bind(wanted)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((id, true)) = written {
            let item = links.as_ref().and_then(|links| links.last());
            record(
                &mut tx,
                NewEvent {
                    kind: "request.created",
                    actor: Some(user),
                    subject: Some(user),
                    document: item.and_then(|link| document_of(&link.node)),
                    node: Some(node),
                    detail: json!({ "request": id }),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The open requests on live items where `manager` has `manage_access` (every live item
    /// for an administrator), newest first, leaving out those of disabled requesters. Empty
    /// for a disabled or unknown account.
    pub async fn open_requests(&self, manager: UserId) -> Result<Vec<AccessRequest>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(admin) = admin_or_none(&mut conn, manager).await? else {
            return Ok(Vec::new());
        };
        let query = format!(
            "with recursive {} \
             select r.id, r.node_id, r.message, r.created_at, u.id as user_id, u.name \
             from managed m join access_requests r on r.id = m.id \
             join users u on u.id = r.requester \
             order by r.created_at desc, r.id",
            managed_requests()
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(manager.0)
            .bind(admin)
            .fetch_all(&mut *conn)
            .await?;
        let mut requests = Vec::with_capacity(rows.len());
        for row in &rows {
            let node = NodeId(row.try_get("node_id")?);
            // The one access decision has the last word; the query only narrows the search.
            let Some(mut links) = chain(&mut conn, manager, node).await? else {
                continue;
            };
            if !links
                .last()
                .and_then(|link| link.role.as_ref())
                .is_some_and(crate::Access::can_manage_access)
            {
                continue;
            }
            let Some(item) = links.pop() else {
                continue;
            };
            requests.push(AccessRequest {
                id: row.try_get("id")?,
                node: crumb(&item),
                path: links.iter().map(crumb).collect(),
                requester: UserRef {
                    id: UserId(row.try_get("user_id")?),
                    name: row.try_get("name")?,
                },
                message: row.try_get("message")?,
                created_at: row.try_get("created_at")?,
            });
        }
        Ok(requests)
    }

    /// How many open requests [`Db::open_requests`] would list for `manager`, in one query.
    pub async fn open_request_count(&self, manager: UserId) -> Result<i64, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(admin) = admin_or_none(&mut conn, manager).await? else {
            return Ok(0);
        };
        let query = format!(
            "with recursive {} select count(*) from managed",
            managed_requests()
        );
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .bind(manager.0)
            .bind(admin)
            .fetch_one(&mut *conn)
            .await?)
    }

    /// Approves the open request `id`, giving its requester the role `role` (a role id)
    /// directly on its item, through [`Db::set_node_access`]'s path: `actor` must manage access
    /// to the item (`NotFound` for an unknown request or one they cannot see, `Forbidden`),
    /// and may only give permissions they hold there (`BeyondYourAccess`); `NoSuchRole` for an
    /// unknown role; inheritance is broken first if the item inherits. `AlreadyDecided` if the
    /// request was decided already (or closed by its item's deletion). Never a downgrade:
    /// `AlreadyHas`, leaving the request open, when the requester's access already covers the
    /// role, or when the role would replace an assignment of theirs on the item's scope that
    /// gives a permission the role lacks. Records the assignment's event and
    /// `request.approved`.
    ///
    /// Locks the item's chain, the groups and the roles as [`Db::set_node_access`] does, then
    /// the request's row `FOR UPDATE`: an approval and a decline of one request take turns,
    /// and the second finds it decided.
    pub async fn approve_request(
        &self,
        actor: UserId,
        id: Uuid,
        role: &str,
    ) -> Result<(), AccessError> {
        let (node, requester) = self.request_item(actor, id).await?;
        let (mut tx, links, _) = self
            .lock_chain(actor, node, false, Some(role), None, false)
            .await?;
        let links = manager(Some(links))?;
        lock_open_request(&mut tx, id).await?;
        let given = crate::roles::role_by_id(&mut tx, role)
            .await?
            .ok_or(AccessError::NoSuchRole)?;
        if let Some(held) = links.last().and_then(|link| link.role.as_ref())
            && !held.perms.contains(given.perms)
        {
            return Err(AccessError::BeyondYourAccess);
        }
        let covered = crate::scopes::access_in(&mut tx, requester, node)
            .await?
            .is_some_and(|held| held.perms.contains(given.perms));
        let source = links
            .get(source_index(&links, false))
            .ok_or(AccessError::NotFound)?
            .node
            .id;
        let replaced_gives_more = assignment_on(&mut tx, source, Principal::User(requester))
            .await?
            .is_some_and(|(_, perms)| !given.perms.contains(perms));
        if covered || replaced_gives_more {
            return Err(AccessError::AlreadyHas);
        }
        assign_locked(
            &mut tx,
            actor,
            node,
            links.clone(),
            Principal::User(requester),
            Some(role),
        )
        .await?;
        sqlx::query(
            "update access_requests set decided_at = now(), decided_by = $2, \
                 decision = 'approved', role_id = $3 \
             where id = $1",
        )
        .bind(id)
        .bind(actor.0)
        .bind(&given.id)
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "request.approved",
                actor: Some(actor),
                subject: Some(requester),
                document: links.last().and_then(|link| document_of(&link.node)),
                node: Some(node),
                detail: json!({ "request": id, "role": given.id }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Declines the open request `id`, with an optional `reply` (trimmed; at most
    /// [`REQUEST_MESSAGE_MAX`] characters, `MessageTooLong`) that its requester sees. `actor`
    /// must manage access to the item (`NotFound`, `Forbidden`); `AlreadyDecided` if it was
    /// decided already. Records `request.declined`. Locks as [`Db::approve_request`] does.
    pub async fn decline_request(
        &self,
        actor: UserId,
        id: Uuid,
        reply: Option<&str>,
    ) -> Result<(), AccessError> {
        let reply = checked_message(reply).ok_or(AccessError::MessageTooLong)?;
        let (node, requester) = self.request_item(actor, id).await?;
        let (mut tx, links, _) = self
            .lock_chain(actor, node, false, None, None, false)
            .await?;
        let links = manager(Some(links))?;
        lock_open_request(&mut tx, id).await?;
        sqlx::query(
            "update access_requests set decided_at = now(), decided_by = $2, \
                 decision = 'declined', reply = $3 \
             where id = $1",
        )
        .bind(id)
        .bind(actor.0)
        .bind(reply.as_deref())
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "request.declined",
                actor: Some(actor),
                subject: Some(requester),
                document: links.last().and_then(|link| document_of(&link.node)),
                node: Some(node),
                detail: json!({ "request": id, "replied": reply.is_some() }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// `user`'s own requests that someone approved or declined, most recently decided first,
    /// each naming its item only while they can see it (have access to it or below it). Open
    /// requests, and those closed because their item was deleted, are never listed: only a
    /// manager's decision says that an item exists.
    pub async fn my_requests(&self, user: UserId) -> Result<Vec<MyRequest>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let rows = sqlx::query(
            "select r.id, r.node_id, r.decision, r.reply, r.created_at, r.decided_at, \
                    ro.id as role_id, ro.name as role_name \
             from access_requests r left join roles ro on ro.id = r.role_id \
             where r.requester = $1 and r.decision in ('approved', 'declined') \
             order by r.decided_at desc, r.id",
        )
        .bind(user.0)
        .fetch_all(&mut *conn)
        .await?;
        let mut mine = Vec::with_capacity(rows.len());
        for row in &rows {
            let node = NodeId(row.try_get("node_id")?);
            let decision = match row.try_get::<String, _>("decision")?.as_str() {
                "approved" => RequestDecision::Approved,
                _ => RequestDecision::Declined,
            };
            let role = match row.try_get::<Option<String>, _>("role_id")? {
                Some(id) => Some(RoleRef {
                    id,
                    name: row.try_get("role_name")?,
                }),
                None => None,
            };
            let mut path = path_in(&mut conn, user, node).await?.unwrap_or_default();
            let item = path.pop();
            mine.push(MyRequest {
                id: row.try_get("id")?,
                node: item,
                path,
                decision,
                role,
                reply: row.try_get("reply")?,
                created_at: row.try_get("created_at")?,
                decided_at: row.try_get("decided_at")?,
            });
        }
        Ok(mine)
    }

    /// Deletes the records of submissions made at or before `now` minus a day: the daily limit
    /// no longer counts them. For the sweeper. Returns how many it deleted.
    pub async fn forget_old_attempts(&self, now: OffsetDateTime) -> Result<u64, DbError> {
        Ok(
            sqlx::query("delete from request_attempts where at <= $1 - interval '1 day'")
                .bind(now)
                .execute(self.pool())
                .await?
                .rows_affected(),
        )
    }

    /// The item and requester of request `id`, if `actor` may decide it: `NotFound` for an
    /// unknown request or an item they cannot see, `Forbidden` without `manage_access` there.
    /// A cheap early refusal; the check under the locks is the one that counts.
    async fn request_item(&self, actor: UserId, id: Uuid) -> Result<(NodeId, UserId), AccessError> {
        let mut conn = self.pool().acquire().await?;
        let row: Option<(Uuid, Uuid)> =
            sqlx::query_as("select node_id, requester from access_requests where id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        let (node, requester) = row.ok_or(AccessError::NotFound)?;
        let node = NodeId(node);
        manager(chain(&mut conn, actor, node).await?)?;
        Ok((node, UserId(requester)))
    }
}

/// Locks the request `id`'s row `FOR UPDATE`, after everything else the transaction locks;
/// `AlreadyDecided` unless it is still open.
async fn lock_open_request(conn: &mut PgConnection, id: Uuid) -> Result<(), AccessError> {
    let open: Option<bool> = sqlx::query_scalar(
        "select decided_at is null from access_requests where id = $1 for update",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    match open {
        Some(true) => Ok(()),
        Some(false) => Err(AccessError::AlreadyDecided),
        None => Err(AccessError::NotFound),
    }
}
