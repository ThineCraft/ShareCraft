#![forbid(unsafe_code)]
//! The PostgreSQL records: accounts, sessions, documents, access grants, the revision index
//! and the event log. PostgreSQL never holds document content; Git does.

mod db;
#[cfg(feature = "test-support")]
pub mod test_support;

pub use db::{Db, DbError};
