#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use records::test_support::TestDb;
use sqlx::Row;
use uuid::Uuid;

const TABLES: [&str; 6] = [
    "users",
    "sessions",
    "documents",
    "document_access",
    "revisions",
    "events",
];

async fn pool(db: &records::Db) -> sqlx::PgPool {
    db.test_pool().clone()
}

#[tokio::test]
async fn migrated_schema_has_every_table() {
    let test = TestDb::create().await;
    let pool = pool(&test.db).await;
    for table in TABLES {
        let row = sqlx::query(
            "select count(*) as n from information_schema.tables \
             where table_schema = current_schema() and table_name = $1",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("n"), 1, "table {table} is missing");
    }
}

#[tokio::test]
async fn two_test_databases_do_not_see_each_other() {
    let first = TestDb::create().await;
    let second = TestDb::create().await;
    sqlx::query("insert into users (id, email, name, password_hash) values ($1, $2, $3, $4)")
        .bind(Uuid::new_v4())
        .bind("a@example.com")
        .bind("A")
        .bind("not a real hash")
        .execute(first.db.test_pool())
        .await
        .unwrap();
    let count = |db: &records::Db| {
        let pool = db.test_pool().clone();
        async move {
            sqlx::query("select count(*) as n from users")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get::<i64, _>("n")
        }
    };
    assert_eq!(count(&first.db).await, 1);
    assert_eq!(count(&second.db).await, 0);
    // A second pool on the same schema sees the same rows.
    let again = first.connect_again().await;
    assert_eq!(count(&again).await, 1);
}

/// A migration is recognised by a checksum of its bytes, so a carriage return picked up from a
/// checkout changes it and a database migrated from the LF copy refuses to start.
#[test]
fn migration_files_have_no_carriage_returns() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.contains(&b'\r'),
            "{} contains a carriage return: check that .gitattributes pins \
             crates/records/migrations/*.sql to eol=lf, then check the file out again \
             (git rm --cached and git checkout)",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no migrations found in {}", dir.display());
}

#[tokio::test]
async fn debug_output_hides_the_url() {
    let test = TestDb::create().await;
    let shown = format!("{:?}", test.db);
    let url = test.url();
    assert!(!shown.contains(&url));
    assert!(!shown.contains("postgres://"));
    let shown = format!("{test:?}");
    assert!(!shown.contains(&url));
    assert!(!shown.contains("postgres://"));
    let error = records::Db::connect("postgres://nobody:hunter2@127.0.0.1:1/none")
        .await
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("hunter2"));
}
