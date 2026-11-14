#![forbid(unsafe_code)]
//! The PostgreSQL records: accounts, sessions, documents, roles and access assignments, the
//! revision index, the content store's safety rows and the event log. PostgreSQL never holds
//! document content; Git and the content store do.

mod accounts;
mod adoption;
mod blobs;
mod db;
mod documents;
mod events;
mod explorer;
mod groups;
mod links;
mod requests;
mod revisions;
mod roles;
mod scopes;
mod settings;
#[cfg(feature = "test-support")]
pub mod test_support;
mod tree;
mod uploads;

pub use accounts::{
    AccountError, NewUser, SESSION_LIFETIME, Session, SessionToken, SignInError, User,
};
pub use adoption::{Adoption, Unplaced};
pub use blobs::{BLOB_REFS_DIRTY, BLOB_REFS_UPGRADE, ClaimId, StaleClaim};
pub use db::{Db, DbError};
pub use documents::{AccessError, Document, Editor, Grant, validate_document_name};
pub use events::{Event, NewEvent};
pub use explorer::{
    AssignmentView, Break, ExplorerFilter, PrincipalView, RestorePreview, UserRef, breaks_csv,
    is_explorer_cursor,
};
pub use groups::{Group, SiteGroup};
pub use links::{AnyoneGrant, Link, LinkError, LinkKind, LinkToken};
pub use requests::{
    AccessRequest, MyRequest, REQUEST_MESSAGE_MAX, REQUESTS_PER_DAY, RequestDecision, RequestError,
};
pub use revisions::{RevisionRecord, RevisionRow};
pub use roles::{Access, Perms, Principal, RoleDef, RoleRef};
pub use scopes::{Crumb, Cursor, Listed, NodeAccess};
pub use settings::{ANYONE_LINK_MAX_DAYS, ANYONE_LINKS, SharingSettings};
pub use tree::{Node, NodeChange, NodeKind, NodeRow, validate_node_name};
pub use uploads::{NewUpload, UPLOADS_PER_PERSON, UploadError, UploadSession, UploadTarget};
