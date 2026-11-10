//! A private schema per test, in the database named by `TEST_DATABASE_URL`. For tests only.
#![allow(clippy::expect_used, clippy::panic)]

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::{AssertSqlSafe, Row};
use uuid::Uuid;

use crate::Db;

const URL_VARIABLE: &str = "TEST_DATABASE_URL";
/// Schemas older than this are left over from a crashed run.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// A freshly migrated schema that only this test uses.
pub struct TestDb {
    pub db: Db,
    schema: String,
    base_url: String,
}

impl fmt::Debug for TestDb {
    /// Shows the schema only: the base URL holds the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestDb")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

impl TestDb {
    /// Creates and migrates a schema named `t_<unix seconds>_<16 hex>`, and drops schemas of
    /// that form more than an hour old.
    ///
    /// Panics, saying so, when `TEST_DATABASE_URL` is not set: a database test is never skipped.
    pub async fn create() -> TestDb {
        let base_url = std::env::var(URL_VARIABLE).unwrap_or_else(|_| {
            panic!("{URL_VARIABLE} is not set; run the tests with `cargo xtask test`")
        });
        let now = now_seconds();
        let hex: String = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(16)
            .collect();
        let schema = format!("t_{now}_{hex}");

        let admin = Db::connect(&base_url)
            .await
            .expect("cannot connect to the test database");
        drop_stale_schemas(&admin, now).await;
        // The name is hexadecimal generated above, so it is safe to format into the statement.
        sqlx::query(AssertSqlSafe(format!("create schema {schema}")))
            .execute(admin.pool())
            .await
            .expect("cannot create the test schema");
        admin.pool().close().await;

        let db = Db::connect(&schema_url(&base_url, &schema))
            .await
            .expect("cannot connect to the test schema");
        db.migrate().await.expect("cannot migrate the test schema");
        TestDb {
            db,
            schema,
            base_url,
        }
    }

    /// A URL for the database that selects this test's schema, for opening a second pool.
    pub fn url(&self) -> String {
        schema_url(&self.base_url, &self.schema)
    }

    /// A second pool on the same schema.
    pub async fn connect_again(&self) -> Db {
        Db::connect(&self.url())
            .await
            .expect("cannot open a second pool on the test schema")
    }
}

fn schema_url(base_url: &str, schema: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!("{base_url}{separator}options=-c%20search_path%3D{schema}")
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The creation time in a test schema name, `None` for any other schema.
fn schema_created_at(name: &str) -> Option<u64> {
    let mut parts = name.split('_');
    let (prefix, seconds, hex, rest) = (parts.next()?, parts.next()?, parts.next()?, parts.next());
    let well_formed = prefix == "t"
        && rest.is_none()
        && hex.len() == 16
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
        && seconds.bytes().all(|b| b.is_ascii_digit());
    if well_formed {
        seconds.parse().ok()
    } else {
        None
    }
}

async fn drop_stale_schemas(db: &Db, now: u64) {
    let rows = sqlx::query("select nspname from pg_namespace where nspname like 't\\_%'")
        .fetch_all(db.pool())
        .await
        .expect("cannot list test schemas");
    for row in rows {
        let name: String = row.get("nspname");
        let Some(created) = schema_created_at(&name) else {
            continue;
        };
        if now.saturating_sub(created) > STALE_AFTER.as_secs() {
            // `schema_created_at` only accepts names made of `t`, digits, hex and underscores.
            // Tests starting together all sweep the same schemas, so a drop can lose a race with
            // another test's drop. The schema is then gone or going, which is all that matters.
            let _ = sqlx::query(AssertSqlSafe(format!(
                "drop schema if exists {name} cascade"
            )))
            .execute(db.pool())
            .await;
        }
    }
}
