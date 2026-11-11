//! Documents and roles. Access to a document is access to its node of the content tree.

use collab_core::ids::{DocId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::accounts::User;
use crate::events::{NewEvent, record};
use crate::tree::validate_node_name;
use crate::{Db, DbError};

pub(crate) const UNIQUE_VIOLATION: &str = "23505";

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

    pub(crate) fn as_str(self) -> &'static str {
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
    #[error("something with that name is already in that place")]
    NameTaken,
    #[error("that kind of item cannot go there")]
    WrongPlace,
    #[error("an item cannot be moved into itself")]
    IntoItself,
    #[error("that folder is not empty")]
    NotEmpty,
    #[error("a site cannot do that")]
    IsSite,
    /// The tree in PostgreSQL no longer matches Git and could not be repaired yet; tree changes
    /// wait until it is.
    #[error("the content tree is being repaired; try again shortly")]
    OutOfStep,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for AccessError {
    fn from(e: sqlx::Error) -> Self {
        AccessError::Db(e.into())
    }
}

/// Checks a document's name: the rule for every node's name (see [`validate_node_name`]), and
/// for a file, its editor's extension with something before it. Returns the trimmed name.
pub fn validate_document_name(name: &str, editor: Editor) -> Result<&str, AccessError> {
    let name = validate_node_name(name)?;
    check_extension(name, editor)?;
    Ok(name)
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

pub(crate) fn parse_editor(text: &str) -> Result<Editor, sqlx::Error> {
    Editor::from_id(text)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown editor {text:?}").into()))
}

pub(crate) fn parse_role(text: &str) -> Result<Role, sqlx::Error> {
    Role::parse(text).ok_or_else(|| sqlx::Error::Decode(format!("unknown role {text:?}").into()))
}

pub(crate) fn document_from_row(row: &PgRow) -> Result<Document, sqlx::Error> {
    Ok(Document {
        id: DocId(row.try_get("id")?),
        name: row.try_get("name")?,
        editor: parse_editor(&row.try_get::<String, _>("editor")?)?,
        created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
        revision: row.try_get("revision")?,
        revised_at: row.try_get("revised_at")?,
    })
}

impl Db {
    /// Records a document. It has no access of its own: it takes its access from the node it
    /// is placed at in the tree.
    pub async fn create_document(
        &self,
        actor: UserId,
        id: DocId,
        name: &str,
        editor: Editor,
    ) -> Result<Document, AccessError> {
        self.create(actor, id, name, editor).await
    }

    async fn create(
        &self,
        actor: UserId,
        id: DocId,
        name: &str,
        editor: Editor,
    ) -> Result<Document, AccessError> {
        let name = validate_document_name(name, editor)?;
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
        record(
            &mut tx,
            NewEvent {
                kind: "document.created",
                actor: Some(actor),
                subject: None,
                document: Some(id),
                node: Some(id.into()),
                detail: json!({ "name": name, "editor": editor.id() }),
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

    /// What `user` may do with `doc`: [`Db::effective_role`] on the document's node. `None`
    /// for a document not yet placed in the tree.
    pub async fn role(&self, user: UserId, doc: DocId) -> Result<Option<Role>, DbError> {
        self.effective_role(user, doc.into()).await
    }

    /// Who has been granted access to `doc`: the assignments on the scope that decides it.
    /// `actor` must be an owner of it.
    pub async fn grants(&self, actor: UserId, doc: DocId) -> Result<Vec<Grant>, AccessError> {
        Ok(self.access(actor, doc.into()).await?.grants)
    }

    /// [`Db::set_node_access`] on the document's node.
    pub async fn set_access(
        &self,
        actor: UserId,
        doc: DocId,
        user: UserId,
        role: Option<Role>,
    ) -> Result<(Option<Role>, Option<Role>), AccessError> {
        self.set_node_access(actor, doc.into(), user, role).await
    }
}
