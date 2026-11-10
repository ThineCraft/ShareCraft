//! Documents, roles and per-document access grants.

use collab_core::ids::{DocId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::accounts::{User, user_from_row};
use crate::events::{NewEvent, record};
use crate::{Db, DbError};

const MAX_NAME_CHARS: usize = 120;
const UNIQUE_VIOLATION: &str = "23505";

/// What a person may do with a document. Ordered from least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Owner,
}

impl Role {
    /// Editors and owners may change the text.
    #[must_use]
    pub fn can_edit(self) -> bool {
        self >= Role::Editor
    }

    /// Only owners may manage who has access.
    #[must_use]
    pub fn can_manage(self) -> bool {
        self == Role::Owner
    }

    fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }

    fn parse(text: &str) -> Option<Role> {
        match text {
            "viewer" => Some(Role::Viewer),
            "editor" => Some(Role::Editor),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }
}

/// The editor that opens a document. `Text` is the built-in plain-text editor; every other
/// editor works on a file whose bytes ShareCraft never interprets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Editor {
    Text,
    GridCraft,
    WordCraft,
    PdfCraft,
    SolidCraft,
}

impl Editor {
    /// Every editor.
    pub const ALL: [Editor; 5] = [
        Editor::Text,
        Editor::GridCraft,
        Editor::WordCraft,
        Editor::PdfCraft,
        Editor::SolidCraft,
    ];

    /// The lower-case id, as stored and as written in JSON.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Editor::Text => "text",
            Editor::GridCraft => "gridcraft",
            Editor::WordCraft => "wordcraft",
            Editor::PdfCraft => "pdfcraft",
            Editor::SolidCraft => "solidcraft",
        }
    }

    /// The editor with this id.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Editor> {
        Editor::ALL.into_iter().find(|editor| editor.id() == id)
    }

    /// The file extension (with its dot) of the files this editor opens; `None` for `Text`.
    #[must_use]
    pub fn native_extension(self) -> Option<&'static str> {
        match self {
            Editor::Text => None,
            Editor::GridCraft => Some(".xlsx"),
            Editor::WordCraft => Some(".docx"),
            Editor::PdfCraft => Some(".pdf"),
            Editor::SolidCraft => Some(".solidcraft.json"),
        }
    }

    /// The editor whose native extension ends `name`, ignoring ASCII case. Never `Text`.
    #[must_use]
    pub fn for_file_name(name: &str) -> Option<Editor> {
        Editor::ALL.into_iter().find(|editor| {
            editor
                .native_extension()
                .is_some_and(|ext| ends_with(name, ext))
        })
    }

    /// Whether the document is a file ShareCraft keeps without interpreting: every editor but
    /// `Text`.
    #[must_use]
    pub fn is_file(self) -> bool {
        self != Editor::Text
    }
}

/// Whether `name` ends with `suffix`, ignoring ASCII case.
fn ends_with(name: &str, suffix: &str) -> bool {
    name.len()
        .checked_sub(suffix.len())
        .and_then(|start| name.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// A document's record. The content lives in Git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub id: DocId,
    pub name: String,
    pub editor: Editor,
    pub created_by: Option<UserId>,
    /// The current Git revision; `None` until first saved.
    pub revision: Option<String>,
    pub revised_at: Option<OffsetDateTime>,
}

/// One person's explicit access to a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub user: User,
    pub role: Role,
}

/// Why a document or access change failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    /// No such document, or the caller has no access to it; deliberately not distinguished.
    #[error("no such document")]
    NotFound,
    #[error("no such account")]
    NoSuchUser,
    #[error("you may not do that")]
    Forbidden,
    #[error("a document must keep at least one owner")]
    LastOwner,
    #[error("that is not a valid document name")]
    InvalidName,
    #[error("the name of this kind of document must end with its file extension")]
    WrongExtension,
    #[error("a document with that id already exists")]
    Exists,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for AccessError {
    fn from(e: sqlx::Error) -> Self {
        AccessError::Db(e.into())
    }
}

fn validate_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let length = name.chars().count();
    let well_formed = (1..=MAX_NAME_CHARS).contains(&length)
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\');
    if well_formed {
        Ok(name)
    } else {
        Err(AccessError::InvalidName)
    }
}

/// A file document's name must be the editor's extension with something before it.
fn check_extension(name: &str, editor: Editor) -> Result<(), AccessError> {
    match editor.native_extension() {
        Some(ext) if !(ends_with(name, ext) && name.len() > ext.len()) => {
            Err(AccessError::WrongExtension)
        }
        _ => Ok(()),
    }
}

fn parse_editor(text: &str) -> Result<Editor, sqlx::Error> {
    Editor::from_id(text)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown editor {text:?}").into()))
}

fn parse_role(text: &str) -> Result<Role, sqlx::Error> {
    Role::parse(text).ok_or_else(|| sqlx::Error::Decode(format!("unknown role {text:?}").into()))
}

fn document_from_row(row: &PgRow) -> Result<Document, sqlx::Error> {
    Ok(Document {
        id: DocId(row.try_get("id")?),
        name: row.try_get("name")?,
        editor: parse_editor(&row.try_get::<String, _>("editor")?)?,
        created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
        revision: row.try_get("revision")?,
        revised_at: row.try_get("revised_at")?,
    })
}

/// The one access decision. The documents `user` can reach, with the role each gives: nothing
/// for a disabled or unknown account; owner of everything for an enabled administrator;
/// otherwise the grant. `only` narrows it to one document. Everything else that needs a
/// role (`role`, `documents_for`, `set_access`, `grants`) goes through here.
async fn reachable(
    conn: &mut PgConnection,
    user: UserId,
    only: Option<DocId>,
) -> Result<Vec<(Document, Role)>, sqlx::Error> {
    let rows = sqlx::query(
        "select d.id, d.name, d.editor, d.created_by, d.revision, d.revised_at, \
                case when u.is_admin then 'owner' else a.role end as role \
         from users u \
         cross join documents d \
         left join document_access a on a.document_id = d.id and a.user_id = u.id \
         where u.id = $1 and u.disabled_at is null \
           and ($2::uuid is null or d.id = $2) \
           and (u.is_admin or a.role is not null) \
         order by d.created_at desc, d.id desc",
    )
    .bind(user.0)
    .bind(only.map(|d| d.0))
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|row| {
            let role: String = row.try_get("role")?;
            Ok((document_from_row(row)?, parse_role(&role)?))
        })
        .collect()
}

async fn role_on(
    conn: &mut PgConnection,
    user: UserId,
    doc: DocId,
) -> Result<Option<Role>, sqlx::Error> {
    Ok(reachable(conn, user, Some(doc))
        .await?
        .into_iter()
        .next()
        .map(|(_, role)| role))
}

/// Fails with `NotFound` if `actor` has no role on `doc`, `Forbidden` if it is not owner.
async fn require_owner(
    conn: &mut PgConnection,
    actor: UserId,
    doc: DocId,
) -> Result<(), AccessError> {
    match role_on(conn, actor, doc).await? {
        None => Err(AccessError::NotFound),
        Some(role) if role.can_manage() => Ok(()),
        Some(_) => Err(AccessError::Forbidden),
    }
}

impl Db {
    /// Creates a document and makes `actor` its owner.
    pub async fn create_document(
        &self,
        actor: UserId,
        id: DocId,
        name: &str,
        editor: Editor,
    ) -> Result<Document, AccessError> {
        self.create(actor, id, name, editor, None).await
    }

    /// Like [`Db::create_document`], for a document made from a file of `bytes` bytes that
    /// `actor` uploaded: its `document.created` event says so.
    pub async fn create_uploaded_document(
        &self,
        actor: UserId,
        id: DocId,
        name: &str,
        editor: Editor,
        bytes: usize,
    ) -> Result<Document, AccessError> {
        self.create(actor, id, name, editor, Some(bytes)).await
    }

    async fn create(
        &self,
        actor: UserId,
        id: DocId,
        name: &str,
        editor: Editor,
        uploaded: Option<usize>,
    ) -> Result<Document, AccessError> {
        let name = validate_name(name)?;
        check_extension(name, editor)?;
        let mut tx = self.pool().begin().await?;
        let disabled: Option<bool> =
            sqlx::query_scalar("select disabled_at is not null from users where id = $1")
                .bind(actor.0)
                .fetch_optional(&mut *tx)
                .await?;
        match disabled {
            None => return Err(AccessError::NoSuchUser),
            Some(true) => return Err(AccessError::Forbidden),
            Some(false) => {}
        }
        let inserted = sqlx::query(
            "insert into documents (id, name, editor, created_by) values ($1, $2, $3, $4) \
             returning id, name, editor, created_by, revision, revised_at",
        )
        .bind(id.0)
        .bind(name)
        .bind(editor.id())
        .bind(actor.0)
        .fetch_one(&mut *tx)
        .await;
        let row = match inserted {
            Ok(row) => row,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                return Err(AccessError::Exists);
            }
            Err(e) => return Err(e.into()),
        };
        let document = document_from_row(&row)?;
        sqlx::query(
            "insert into document_access (document_id, user_id, role, granted_by) \
             values ($1, $2, 'owner', $2)",
        )
        .bind(id.0)
        .bind(actor.0)
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "document.created",
                actor: Some(actor),
                subject: None,
                document: Some(id),
                detail: match uploaded {
                    Some(bytes) => json!({
                        "name": name,
                        "editor": editor.id(),
                        "uploaded": true,
                        "bytes": bytes,
                    }),
                    None => json!({ "name": name, "editor": editor.id() }),
                },
            },
        )
        .await?;
        tx.commit().await?;
        Ok(document)
    }

    /// The document with this id, whoever may see it. Callers check `role` first.
    pub async fn document(&self, id: DocId) -> Result<Option<Document>, DbError> {
        let row = sqlx::query(
            "select id, name, editor, created_by, revision, revised_at from documents where id = $1",
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(document_from_row).transpose()?)
    }

    /// What `user` may do with `doc`: the single access decision. `None` for a disabled or
    /// unknown account or a missing document; `Owner` for an enabled administrator; otherwise
    /// the grant, if any.
    pub async fn role(&self, user: UserId, doc: DocId) -> Result<Option<Role>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(role_on(&mut conn, user, doc).await?)
    }

    /// The documents `user` has a role on, newest first.
    pub async fn documents_for(&self, user: UserId) -> Result<Vec<(Document, Role)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(reachable(&mut conn, user, None).await?)
    }

    /// Who has been granted access to `doc`. `actor` must be an owner of it.
    pub async fn grants(&self, actor: UserId, doc: DocId) -> Result<Vec<Grant>, AccessError> {
        let mut conn = self.pool().acquire().await?;
        require_owner(&mut conn, actor, doc).await?;
        let rows = sqlx::query(
            "select u.id, u.email, u.name, u.is_admin, (u.disabled_at is not null) as disabled, \
             a.role \
             from document_access a join users u on u.id = a.user_id \
             where a.document_id = $1 order by u.name, u.id",
        )
        .bind(doc.0)
        .fetch_all(&mut *conn)
        .await?;
        let grants = rows
            .iter()
            .map(|row| {
                let role: String = row.try_get("role")?;
                Ok(Grant {
                    user: user_from_row(row)?,
                    role: parse_role(&role)?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()?;
        Ok(grants)
    }

    /// Gives `user` the role `role` on `doc`, or removes their grant when it is `None`.
    /// `actor` must be an owner. Returns the person's role before and after, as `role` would
    /// report it (an administrator stays `Owner` either way).
    ///
    /// The actor's permission is decided with the document locked, so concurrent changes take
    /// turns, an owner removed by an earlier change gets `NotFound`, and the last owner grant
    /// cannot be removed by two calls at once.
    pub async fn set_access(
        &self,
        actor: UserId,
        doc: DocId,
        user: UserId,
        role: Option<Role>,
    ) -> Result<(Option<Role>, Option<Role>), AccessError> {
        let mut tx = self.pool().begin().await?;
        // Cheap early refusal so that people with no access never wait for the lock. Not
        // authoritative: the check under the lock below is.
        require_owner(&mut tx, actor, doc).await?;
        // The document row is the one lock every access change takes first, so changes to
        // one document are serialised and cannot deadlock.
        let locked: Option<Uuid> =
            sqlx::query_scalar("select id from documents where id = $1 for update")
                .bind(doc.0)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            return Err(AccessError::NotFound);
        }
        // Authoritative: judged with the document locked, so an owner removed by a call that
        // just committed cannot make one more change.
        require_owner(&mut tx, actor, doc).await?;
        let exists: Option<Uuid> = sqlx::query_scalar("select id from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AccessError::NoSuchUser);
        }
        let current: Option<String> = sqlx::query_scalar(
            "select role from document_access where document_id = $1 and user_id = $2",
        )
        .bind(doc.0)
        .bind(user.0)
        .fetch_optional(&mut *tx)
        .await?;
        let current = current.as_deref().map(parse_role).transpose()?;
        let before = role_on(&mut tx, user, doc).await?;
        if current == role {
            return Ok((before, before));
        }
        if current == Some(Role::Owner) && role != Some(Role::Owner) {
            let owners: i64 = sqlx::query_scalar(
                "select count(*) from document_access where document_id = $1 and role = 'owner'",
            )
            .bind(doc.0)
            .fetch_one(&mut *tx)
            .await?;
            if owners <= 1 {
                return Err(AccessError::LastOwner);
            }
        }
        let (kind, detail) = match (current, role) {
            (None, Some(new)) => {
                sqlx::query(
                    "insert into document_access (document_id, user_id, role, granted_by) \
                     values ($1, $2, $3, $4)",
                )
                .bind(doc.0)
                .bind(user.0)
                .bind(new.as_str())
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
                ("access.granted", json!({ "role": new.as_str() }))
            }
            (Some(old), Some(new)) => {
                sqlx::query(
                    "update document_access set role = $3, granted_by = $4, granted_at = now() \
                     where document_id = $1 and user_id = $2",
                )
                .bind(doc.0)
                .bind(user.0)
                .bind(new.as_str())
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
                (
                    "access.changed",
                    json!({ "from": old.as_str(), "to": new.as_str() }),
                )
            }
            (Some(old), None) => {
                sqlx::query("delete from document_access where document_id = $1 and user_id = $2")
                    .bind(doc.0)
                    .bind(user.0)
                    .execute(&mut *tx)
                    .await?;
                ("access.revoked", json!({ "role": old.as_str() }))
            }
            (None, None) => return Ok((before, before)),
        };
        record(
            &mut tx,
            NewEvent {
                kind,
                actor: Some(actor),
                subject: Some(user),
                document: Some(doc),
                detail,
            },
        )
        .await?;
        let after = role_on(&mut tx, user, doc).await?;
        tx.commit().await?;
        Ok((before, after))
    }
}
