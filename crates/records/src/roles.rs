//! Roles as named sets of permissions, and the access a person holds on a node. A role is a set
//! of permissions from a fixed list; Viewer, Editor and Owner are built in and never change,
//! and administrators define the rest. An assignment gives a principal a role on a scope.

use std::fmt;

use collab_core::ids::{GroupId, UserId};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::documents::UNIQUE_VIOLATION;
use crate::events::{NewEvent, record};
use crate::scopes::scopes_managed_only_by;
use crate::{AccessError, Db, DbError};

const MAX_ROLE_NAME_CHARS: usize = 60;

/// Each permission's name and bit, in the order of the fixed list.
const NAMES: [(&str, u16); 6] = [
    ("view", 1),
    ("add", 1 << 1),
    ("edit", 1 << 2),
    ("delete", 1 << 3),
    ("manage_access", 1 << 4),
    ("manage_site", 1 << 5),
];
const VIEW_BIT: u16 = 1;
const ALL_BITS: u16 = (1 << 6) - 1;

/// A set of permissions from the fixed list: `view` (list, open, download, join a session
/// read-only), `add` (create folders and files, upload, move into), `edit` (change content,
/// rename, move out, take the editing seat), `delete`, `manage_access` (see and change access,
/// break and restore inheritance, use the explorer) and `manage_site` (create and rename
/// libraries, rename the site, manage the site's groups). Every permission but `view`
/// includes `view`: each constant other than [`Perms::VIEW`] carries it, and every non-empty
/// set built from bits or names gets it. Serialised as the list of its names, in that order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Perms(u16);

impl Perms {
    /// No permission at all.
    pub const NONE: Perms = Perms(0);
    pub const VIEW: Perms = Perms(VIEW_BIT);
    pub const ADD: Perms = Perms(VIEW_BIT | 1 << 1);
    pub const EDIT: Perms = Perms(VIEW_BIT | 1 << 2);
    pub const DELETE: Perms = Perms(VIEW_BIT | 1 << 3);
    pub const MANAGE_ACCESS: Perms = Perms(VIEW_BIT | 1 << 4);
    pub const MANAGE_SITE: Perms = Perms(VIEW_BIT | 1 << 5);
    /// Every permission.
    pub const ALL: Perms = Perms(ALL_BITS);

    /// The set with these bits, with `view` added unless it is empty. `None` if a bit is not a
    /// permission.
    #[must_use]
    pub fn from_bits(bits: u16) -> Option<Perms> {
        if bits & !ALL_BITS != 0 {
            None
        } else if bits == 0 {
            Some(Perms::NONE)
        } else {
            Some(Perms(bits | VIEW_BIT))
        }
    }

    /// The set with these names, with `view` added unless it is empty. `None` if a name is not
    /// a permission.
    pub fn from_names<I, S>(names: I) -> Option<Perms>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut bits = 0;
        for name in names {
            let (_, bit) = NAMES.iter().find(|(known, _)| *known == name.as_ref())?;
            bits |= bit;
        }
        Perms::from_bits(bits)
    }

    /// The bits of the set, as stored.
    #[must_use]
    pub fn bits(self) -> u16 {
        self.0
    }

    /// The names of the permissions in the set, in the order of the fixed list.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        NAMES
            .iter()
            .filter(|(_, bit)| self.0 & bit != 0)
            .map(|(name, _)| *name)
            .collect()
    }

    /// Whether every permission of `other` is in the set.
    #[must_use]
    pub fn contains(self, other: Perms) -> bool {
        self.0 & other.0 == other.0
    }

    /// The permissions in either set.
    #[must_use]
    pub fn union(self, other: Perms) -> Perms {
        Perms(self.0 | other.0)
    }

    /// Whether the set holds no permission.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The set as stored in a `perms` column.
    pub(crate) fn column(self) -> i32 {
        i32::from(self.0)
    }

    /// The set stored in a `perms` column.
    pub(crate) fn from_column(value: i32) -> Result<Perms, sqlx::Error> {
        u16::try_from(value)
            .ok()
            .and_then(Perms::from_bits)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown permissions {value}").into()))
    }
}

impl Serialize for Perms {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let names = self.names();
        let mut seq = serializer.serialize_seq(Some(names.len()))?;
        for name in names {
            seq.serialize_element(name)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Perms {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Perms, D::Error> {
        struct Names;

        impl<'de> Visitor<'de> for Names {
            type Value = Perms;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of permission names")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Perms, A::Error> {
                let mut names = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    names.push(name);
                }
                Perms::from_names(&names)
                    .ok_or_else(|| de::Error::custom("unknown permission name"))
            }
        }

        deserializer.deserialize_seq(Names)
    }
}

/// A role: a named set of permissions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoleDef {
    /// `viewer`, `editor` or `owner` for a built-in role; generated for any other.
    pub id: String,
    pub name: String,
    pub perms: Perms,
    /// Whether the role is built in, and so cannot be changed or deleted.
    pub builtin: bool,
}

impl RoleDef {
    /// The built-in role with this id: `viewer` {view}, `editor` {view, add, edit, delete} or
    /// `owner` {every permission}.
    #[must_use]
    pub fn builtin(id: &str) -> Option<RoleDef> {
        let (name, perms) = match id {
            "viewer" => ("Viewer", Perms::VIEW),
            "editor" => ("Editor", Perms::ADD.union(Perms::EDIT).union(Perms::DELETE)),
            "owner" => ("Owner", Perms::ALL),
            _ => return None,
        };
        Some(RoleDef {
            id: id.to_owned(),
            name: name.to_owned(),
            perms,
            builtin: true,
        })
    }

    /// The role's id and name, for display.
    #[must_use]
    pub fn role_ref(&self) -> RoleRef {
        RoleRef {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }
}

/// A role named for display: its id and its name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct RoleRef {
    pub id: String,
    pub name: String,
}

/// What a person may do with a node: the union of the permissions of every assignment that
/// names them on the scope deciding the node, and the roles that gave them, for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Access {
    pub perms: Perms,
    /// The roles of those assignments, by id, each once.
    pub roles: Vec<RoleRef>,
}

impl Access {
    /// The access one role gives.
    #[must_use]
    pub fn from_role(role: &RoleDef) -> Access {
        Access {
            perms: role.perms,
            roles: vec![role.role_ref()],
        }
    }

    /// An enabled administrator's access: every permission, shown as Owner.
    #[must_use]
    pub fn administrator() -> Access {
        Access {
            perms: Perms::ALL,
            roles: vec![RoleRef {
                id: "owner".to_owned(),
                name: "Owner".to_owned(),
            }],
        }
    }

    /// The union of the access the given assignments give: their roles and permissions. `None`
    /// when there are none.
    pub(crate) fn union_of(given: impl IntoIterator<Item = (RoleRef, Perms)>) -> Option<Access> {
        let mut perms = Perms::NONE;
        let mut roles = Vec::new();
        for (role, role_perms) in given {
            perms = perms.union(role_perms);
            roles.push(role);
        }
        if roles.is_empty() {
            return None;
        }
        roles.sort();
        roles.dedup_by(|a, b| a.id == b.id);
        Some(Access { perms, roles })
    }

    /// Reads the columns `perms` (`bit_or` of the roles' permissions), `role_ids` and
    /// `role_names` (in the same order) that the listings compute. `None` when `perms` is null.
    pub(crate) fn from_aggregate(row: &PgRow) -> Result<Option<Access>, sqlx::Error> {
        let Some(perms) = row.try_get::<Option<i32>, _>("perms")? else {
            return Ok(None);
        };
        let ids: Vec<String> = row
            .try_get::<Option<Vec<String>>, _>("role_ids")?
            .unwrap_or_default();
        let names: Vec<String> = row
            .try_get::<Option<Vec<String>>, _>("role_names")?
            .unwrap_or_default();
        let mut roles: Vec<RoleRef> = ids
            .into_iter()
            .zip(names)
            .map(|(id, name)| RoleRef { id, name })
            .collect();
        roles.sort();
        roles.dedup_by(|a, b| a.id == b.id);
        Ok(Some(Access {
            perms: Perms::from_column(perms)?,
            roles,
        }))
    }

    #[must_use]
    pub fn can_view(&self) -> bool {
        self.perms.contains(Perms::VIEW)
    }

    #[must_use]
    pub fn can_add(&self) -> bool {
        self.perms.contains(Perms::ADD)
    }

    #[must_use]
    pub fn can_edit(&self) -> bool {
        self.perms.contains(Perms::EDIT)
    }

    #[must_use]
    pub fn can_delete(&self) -> bool {
        self.perms.contains(Perms::DELETE)
    }

    #[must_use]
    pub fn can_manage_access(&self) -> bool {
        self.perms.contains(Perms::MANAGE_ACCESS)
    }

    #[must_use]
    pub fn can_manage_site(&self) -> bool {
        self.perms.contains(Perms::MANAGE_SITE)
    }
}

/// Who an assignment gives a role to: a person or a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Principal {
    User(UserId),
    Group(GroupId),
}

impl Principal {
    /// `user` or `group`, as stored in `assignments.principal_kind`.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Principal::User(_) => "user",
            Principal::Group(_) => "group",
        }
    }

    /// The person's or group's id, as stored in `assignments.principal_id`.
    #[must_use]
    pub fn id(self) -> Uuid {
        match self {
            Principal::User(user) => user.0,
            Principal::Group(group) => group.0,
        }
    }
}

const ROLE_COLUMNS: &str = "id, name, perms, builtin";

fn role_from_row(row: &PgRow) -> Result<RoleDef, sqlx::Error> {
    Ok(RoleDef {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        perms: Perms::from_column(row.try_get("perms")?)?,
        builtin: row.try_get("builtin")?,
    })
}

/// The role with this id, on `conn`, without locking it.
pub(crate) async fn role_by_id(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<RoleDef>, sqlx::Error> {
    let query = format!("select {ROLE_COLUMNS} from roles where id = $1");
    let row = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(role_from_row).transpose()
}

/// Locks the roles with these ids `FOR SHARE`, in one pass ordered by id, so that none of them
/// changes before the transaction ends. Ids that are not roles are passed over.
pub(crate) async fn lock_roles(
    conn: &mut PgConnection,
    ids: impl IntoIterator<Item = String>,
) -> Result<(), sqlx::Error> {
    let mut ids: Vec<String> = ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(());
    }
    ids.sort();
    ids.dedup();
    sqlx::query("select id from roles where id = any($1) order by id for share")
        .bind(&ids)
        .execute(conn)
        .await?;
    Ok(())
}

/// `Forbidden` unless `actor` is an enabled administrator; locks their row `FOR SHARE` so that
/// it stays so until the transaction ends.
async fn require_admin(conn: &mut PgConnection, actor: UserId) -> Result<(), AccessError> {
    let admin: Option<bool> = sqlx::query_scalar(
        "select is_admin and disabled_at is null from users where id = $1 for share",
    )
    .bind(actor.0)
    .fetch_optional(conn)
    .await?;
    if admin == Some(true) {
        Ok(())
    } else {
        Err(AccessError::Forbidden)
    }
}

/// Checks a role's name: 1 to 60 characters after trimming, no control characters. Returns the
/// trimmed name.
fn validate_role_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let length = name.chars().count();
    if (1..=MAX_ROLE_NAME_CHARS).contains(&length) && !name.chars().any(char::is_control) {
        Ok(name)
    } else {
        Err(AccessError::InvalidName)
    }
}

/// `NameTaken` for a unique violation (a name another role has, ignoring case), otherwise the
/// error.
fn name_taken(e: sqlx::Error) -> AccessError {
    match e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some(UNIQUE_VIOLATION) => {
            AccessError::NameTaken
        }
        other => other.into(),
    }
}

fn role_event(kind: &'static str, actor: UserId, detail: serde_json::Value) -> NewEvent {
    NewEvent {
        kind,
        actor: Some(actor),
        subject: None,
        document: None,
        node: None,
        detail,
    }
}

impl Db {
    /// Every role: the built-in ones first (Viewer, Editor, Owner), then the others by name.
    pub async fn roles(&self) -> Result<Vec<RoleDef>, DbError> {
        let query = format!(
            "select {ROLE_COLUMNS} from roles \
             order by builtin desc, case when builtin then perms end, lower(name), id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(self.pool())
            .await?;
        rows.iter()
            .map(role_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Defines a role named `name` with `perms` (and `view`, which every role has). `actor`
    /// must be an enabled administrator (`Forbidden`). `InvalidName` for a name that is empty
    /// or longer than 60 characters after trimming, `NameTaken` for one another role has,
    /// ignoring case. Records `role.created`.
    pub async fn create_role(
        &self,
        actor: UserId,
        name: &str,
        perms: Perms,
    ) -> Result<RoleDef, AccessError> {
        let name = validate_role_name(name)?;
        let perms = perms.union(Perms::VIEW);
        let mut tx = self.pool().begin().await?;
        require_admin(&mut tx, actor).await?;
        let id = Uuid::new_v4().to_string();
        let query = format!(
            "insert into roles (id, name, perms, created_by) values ($1, $2, $3, $4) \
             returning {ROLE_COLUMNS}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&id)
            .bind(name)
            .bind(perms.column())
            .bind(actor.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(name_taken)?;
        let role = role_from_row(&row)?;
        record(
            &mut tx,
            role_event(
                "role.created",
                actor,
                json!({ "id": role.id, "name": role.name, "perms": role.perms }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(role)
    }

    /// Renames the role `id` and changes its permissions (each when given; `view` is always
    /// kept). The change reaches everyone holding the role, everywhere, at once. `actor` must
    /// be an enabled administrator (`Forbidden`); `NoSuchRole` for an unknown id,
    /// `BuiltinRole` for a built-in role; names as [`Db::create_role`]; `LastManager` if the
    /// role would lose `manage_access` while it gives the only managing assignment of a scope. Takes the role's row
    /// `FOR UPDATE`, so an access decision judged by the role either finishes before the change
    /// or sees it. Records `role.changed` with the name and permissions before and after.
    pub async fn change_role(
        &self,
        actor: UserId,
        id: &str,
        name: Option<&str>,
        perms: Option<Perms>,
    ) -> Result<RoleDef, AccessError> {
        let name = name.map(validate_role_name).transpose()?;
        let mut tx = self.pool().begin().await?;
        require_admin(&mut tx, actor).await?;
        let before = locked_custom_role(&mut tx, id).await?;
        let after = RoleDef {
            id: before.id.clone(),
            name: name.map_or_else(|| before.name.clone(), str::to_owned),
            perms: perms.map_or(before.perms, |p| p.union(Perms::VIEW)),
            builtin: false,
        };
        if after == before {
            return Ok(before);
        }
        // Every scope keeps a manager. The role's row is held `FOR UPDATE`, and every access
        // change takes the roles assigned on the scope it changes `FOR SHARE` (after its chain),
        // so a change removing another manager on a scope holding this role either committed
        // before this count or waits until this transaction ends and then sees the new
        // permissions. No deadlock: this transaction holds only the actor's row (shared) and
        // this role's row, and waits for nothing else once it has the role; the others wait for
        // it only on this one row.
        if before.perms.contains(Perms::MANAGE_ACCESS)
            && !after.perms.contains(Perms::MANAGE_ACCESS)
            && scopes_managed_only_by(&mut tx, id).await? > 0
        {
            return Err(AccessError::LastManager);
        }
        sqlx::query("update roles set name = $2, perms = $3 where id = $1")
            .bind(id)
            .bind(&after.name)
            .bind(after.perms.column())
            .execute(&mut *tx)
            .await
            .map_err(name_taken)?;
        record(
            &mut tx,
            role_event(
                "role.changed",
                actor,
                json!({
                    "id": id,
                    "before": { "name": before.name, "perms": before.perms },
                    "after": { "name": after.name, "perms": after.perms },
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(after)
    }

    /// Deletes the role `id`. `actor` must be an enabled administrator (`Forbidden`);
    /// `NoSuchRole` for an unknown id, `BuiltinRole` for a built-in role, `RoleInUse` while any
    /// assignment gives it. Records `role.deleted`.
    pub async fn delete_role(&self, actor: UserId, id: &str) -> Result<(), AccessError> {
        let mut tx = self.pool().begin().await?;
        require_admin(&mut tx, actor).await?;
        let role = locked_custom_role(&mut tx, id).await?;
        let in_use: bool =
            sqlx::query_scalar("select exists (select 1 from assignments where role_id = $1)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if in_use {
            return Err(AccessError::RoleInUse);
        }
        sqlx::query("delete from roles where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            role_event(
                "role.deleted",
                actor,
                json!({ "id": role.id, "name": role.name, "perms": role.perms }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// The custom role `id`, locked `FOR UPDATE`: `NoSuchRole` if there is none, `BuiltinRole`
/// for a built-in one (which is never locked: it cannot change).
async fn locked_custom_role(conn: &mut PgConnection, id: &str) -> Result<RoleDef, AccessError> {
    let found = role_by_id(&mut *conn, id)
        .await?
        .ok_or(AccessError::NoSuchRole)?;
    if found.builtin {
        return Err(AccessError::BuiltinRole);
    }
    let query = format!("select {ROLE_COLUMNS} from roles where id = $1 for update");
    let row = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or(AccessError::NoSuchRole)?;
    Ok(role_from_row(&row)?)
}
