//! Rebuilds the crate when the migrations folder changes.
//!
//! `sqlx::migrate!` embeds the migration files at compile time, so a build must not keep stale
//! bytes after a file is added, removed or checked out again with other line endings.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
