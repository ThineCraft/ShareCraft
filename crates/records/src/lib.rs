#![forbid(unsafe_code)]
//! The PostgreSQL records: accounts, sessions, documents, access grants, the revision index
//! and the event log. PostgreSQL never holds document content; Git does.

mod accounts;
mod db;
mod documents;
mod events;
mod revisions;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use accounts::{
    AccountError, NewUser, SESSION_LIFETIME, Session, SessionToken, SignInError, User,
};
pub use db::{Db, DbError};
pub use documents::{AccessError, Document, Editor, Grant, Role};
pub use events::{Event, NewEvent};
pub use revisions::{RevisionRecord, RevisionRow};
