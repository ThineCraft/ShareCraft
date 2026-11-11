#![forbid(unsafe_code)]
//! The PostgreSQL records: accounts, sessions, documents, access grants, the revision index
//! and the event log. PostgreSQL never holds document content; Git does.

mod accounts;
mod db;
mod documents;
mod events;
mod revisions;
mod scopes;
#[cfg(feature = "test-support")]
pub mod test_support;
mod tree;

pub use accounts::{
    AccountError, NewUser, SESSION_LIFETIME, Session, SessionToken, SignInError, User,
};
pub use db::{Db, DbError};
pub use documents::{AccessError, Document, Editor, Grant, Role};
pub use events::{Event, NewEvent};
pub use revisions::{RevisionRecord, RevisionRow};
pub use scopes::{Crumb, Cursor, Listed, NodeAccess};
pub use tree::{Node, NodeKind, NodeRow, validate_node_name};
