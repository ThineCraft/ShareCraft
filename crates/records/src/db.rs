//! The connection pool and migrations.

use std::fmt;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const MAX_CONNECTIONS: u32 = 10;
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// A database failure.
///
/// Carries the underlying error as text, so callers do not depend on the database library used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DbError(String);

impl DbError {
    /// A database failure described by `cause`.
    pub fn new(cause: impl fmt::Display) -> Self {
        DbError(cause.to_string())
    }
}

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        DbError::new(e)
    }
}

impl From<sqlx::migrate::MigrateError> for DbError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        DbError::new(e)
    }
}

/// A pool of connections to the database. Cloning shares the pool.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl fmt::Debug for Db {
    /// Prints no connection details: the URL holds the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Connects to the database at `url`, with at most ten connections.
    pub async fn connect(url: &str) -> Result<Db, DbError> {
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect(url)
            .await?;
        Ok(Db { pool })
    }

    /// Brings the schema up to date.
    pub async fn migrate(&self) -> Result<(), DbError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// The underlying pool, for the queries in this crate and its tests.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
