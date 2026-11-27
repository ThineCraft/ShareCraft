#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{
    AccessError, NewSource, NodeKind, SealedRow, SourceChange, SourceConfig, SourceItem,
    SourceKind, SourceMode, SourceState, SourceStatus,
};
use sqlx::Row;
use time::{Duration, OffsetDateTime};

struct Fixture {
    test: TestDb,
    user: UserId,
    site: NodeId,
    library: NodeId,
}

async fn fixture() -> Fixture {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    let site = site(&test, "Site", Some(ann.id)).await;
    let library = put(&test, new_id(), NodeKind::Library, Some(site), site, "Repo").await;
    Fixture {
        test,
        user: ann.id,
        site,
        library,
    }
}

fn new_source(f: &Fixture, library: NodeId) -> NewSource {
    NewSource {
        library,
        kind: SourceKind::Github,
        owner: "thinecraft".into(),
        repo: "docs".into(),
        branch: "main".into(),
        folder: String::new(),
        location: None,
        mode: SourceMode::ReadOnly,
        token: None,
        poll_seconds: 300,
        connected_by: f.user,
    }
}

fn sealed(bytes: &[u8]) -> SealedRow {
    SealedRow {
        key_id: 1,
        nonce: [7; 12],
        ciphertext: bytes.to_vec(),
    }
}

fn status(state: SourceState) -> SourceStatus {
    SourceStatus {
        state,
        last_commit: None,
        etag: None,
        last_checked_at: None,
        last_synced_at: None,
        last_error: None,
        retry_at: None,
        files: 0,
        bytes: 0,
        hidden: 0,
        hidden_detail: serde_json::json!({}),
        next_check_at: None,
    }
}

#[tokio::test]
async fn connect_then_read_back() {
    let f = fixture().await;
    let token = f.test.db.put_secret(&sealed(b"cipher")).await.unwrap();
    let mut new = new_source(&f, f.library);
    new.folder = "docs".into();
    new.token = Some(token);
    f.test.db.connect_source(&new).await.unwrap();
    let (config, status) = f.test.db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.library, f.library);
    assert_eq!(config.kind, SourceKind::Github);
    assert_eq!(config.owner, "thinecraft");
    assert_eq!(config.repo, "docs");
    assert_eq!(config.branch, "main");
    assert_eq!(config.folder, "docs");
    assert_eq!(config.token, Some(token));
    assert_eq!(config.webhook_secret, None);
    assert_eq!(config.poll_seconds, 300);
    assert_eq!(config.connected_by, f.user);
    assert_eq!(status.state, SourceState::Connecting);
    assert_eq!(status.last_commit, None);
    assert_eq!((status.files, status.bytes, status.hidden), (0, 0, 0));
    let back = f.test.db.get_secret(token).await.unwrap().unwrap();
    assert_eq!(back, sealed(b"cipher"));
    // A secret a connection uses cannot be deleted; one nothing uses can.
    assert!(f.test.db.delete_secret(token).await.is_err());
    let spare = f.test.db.put_secret(&sealed(b"spare")).await.unwrap();
    f.test.db.delete_secret(spare).await.unwrap();
    assert_eq!(f.test.db.get_secret(spare).await.unwrap(), None);
}

#[tokio::test]
async fn an_empty_ciphertext_is_refused() {
    let f = fixture().await;
    assert!(f.test.db.put_secret(&sealed(b"")).await.is_err());
}

#[tokio::test]
async fn connecting_a_non_empty_library_is_refused() {
    let f = fixture().await;
    document_in(&f.test, f.user, f.library, f.site, "Notes.txt").await;
    let result = f.test.db.connect_source(&new_source(&f, f.library)).await;
    assert_eq!(result, Err(AccessError::LibraryNotEmpty));
    assert!(f.test.db.source_of(f.library).await.unwrap().is_none());
}

#[tokio::test]
async fn connecting_something_that_is_not_a_library_is_refused() {
    let f = fixture().await;
    let result = f.test.db.connect_source(&new_source(&f, f.site)).await;
    assert_eq!(result, Err(AccessError::NotFound));
    let result = f.test.db.connect_source(&new_source(&f, new_id())).await;
    assert_eq!(result, Err(AccessError::NotFound));
}

#[tokio::test]
async fn connecting_twice_is_refused() {
    let f = fixture().await;
    f.test
        .db
        .connect_source(&new_source(&f, f.library))
        .await
        .unwrap();
    let result = f.test.db.connect_source(&new_source(&f, f.library)).await;
    assert_eq!(result, Err(AccessError::SourceExists));
}

#[tokio::test]
async fn source_library_of_finds_the_library_for_a_file_deep_inside() {
    let f = fixture().await;
    f.test
        .db
        .connect_source(&new_source(&f, f.library))
        .await
        .unwrap();
    let a = put(
        &f.test,
        new_id(),
        NodeKind::Folder,
        Some(f.library),
        f.site,
        "A",
    )
    .await;
    let b = put(&f.test, new_id(), NodeKind::Folder, Some(a), f.site, "B").await;
    let doc = document_in(&f.test, f.user, b, f.site, "Deep.txt").await;
    let found = |node: NodeId| {
        let db = f.test.db.clone();
        async move { db.source_library_of(node).await.unwrap() }
    };
    assert_eq!(found(doc.into()).await, Some(f.library));
    assert_eq!(found(f.library).await, Some(f.library));
    assert_eq!(found(f.site).await, None);
}

#[tokio::test]
async fn source_library_of_returns_none_for_an_ordinary_library() {
    let f = fixture().await;
    let doc = document_in(&f.test, f.user, f.library, f.site, "Notes.txt").await;
    assert_eq!(f.test.db.source_library_of(doc.into()).await.unwrap(), None);
    assert_eq!(f.test.db.source_library_of(f.library).await.unwrap(), None);
}

#[tokio::test]
async fn changing_branch_clears_the_bookmark() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let mut s = status(SourceState::Ok);
    s.last_commit = Some("abc123".into());
    s.etag = Some("\"tag\"".into());
    db.set_source_status(f.library, &s).await.unwrap();

    // A change of poll time alone keeps the bookmark.
    db.change_source(
        f.library,
        &SourceChange {
            poll_seconds: Some(600),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let (config, kept) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.poll_seconds, 600);
    assert_eq!(kept.last_commit.as_deref(), Some("abc123"));
    assert_eq!(kept.etag.as_deref(), Some("\"tag\""));

    db.change_source(
        f.library,
        &SourceChange {
            branch: Some("dev".into()),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let (config, cleared) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.branch, "dev");
    assert_eq!(cleared.last_commit, None);
    assert_eq!(cleared.etag, None);

    // The same for a folder.
    db.set_source_status(f.library, &s).await.unwrap();
    db.change_source(
        f.library,
        &SourceChange {
            folder: Some("docs".into()),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let (config, cleared) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.folder, "docs");
    assert_eq!(cleared.last_commit, None);
    assert_eq!(cleared.etag, None);

    // Secrets change, and can be taken away.
    let webhook = db.put_secret(&sealed(b"hook")).await.unwrap();
    db.change_source(
        f.library,
        &SourceChange {
            webhook_secret: Some(Some(webhook)),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let (config, _) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.webhook_secret, Some(webhook));
    db.change_source(
        f.library,
        &SourceChange {
            webhook_secret: Some(None),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let (config, _) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.webhook_secret, None);
}

#[tokio::test]
async fn changing_an_unconnected_library_is_not_found() {
    let f = fixture().await;
    let result = f
        .test
        .db
        .change_source(f.library, &SourceChange::default())
        .await;
    assert_eq!(result, Err(AccessError::NotFound));
}

#[tokio::test]
async fn items_round_trip_and_path_is_unique() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let folder = put(
        &f.test,
        new_id(),
        NodeKind::Folder,
        Some(f.library),
        f.site,
        "docs",
    )
    .await;
    let file = document_in(&f.test, f.user, folder, f.site, "a.txt").await;
    let other = document_in(&f.test, f.user, folder, f.site, "b.txt").await;
    db.upsert_source_items(
        f.library,
        &[
            SourceItem {
                node: folder,
                path: "docs".into(),
                repository_path: "docs".into(),
                blob_sha: None,
                size: 0,
            },
            SourceItem {
                node: file.into(),
                path: "docs/a.txt".into(),
                repository_path: "docs/a.txt".into(),
                blob_sha: Some("sha1".into()),
                size: 5,
            },
        ],
    )
    .await
    .unwrap();
    let mut items = db.source_items(f.library).await.unwrap();
    items.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].path, "docs");
    assert_eq!(items[0].blob_sha, None);
    assert_eq!(items[1].blob_sha.as_deref(), Some("sha1"));
    assert_eq!(items[1].size, 5);

    // The same path again replaces its row; it never makes a second.
    db.upsert_source_items(
        f.library,
        &[SourceItem {
            node: file.into(),
            path: "docs/a.txt".into(),
            repository_path: "docs/a.txt".into(),
            blob_sha: Some("sha2".into()),
            size: 9,
        }],
    )
    .await
    .unwrap();
    let items = db.source_items(f.library).await.unwrap();
    assert_eq!(items.len(), 2);
    let a = items.iter().find(|i| i.path == "docs/a.txt").unwrap();
    assert_eq!((a.blob_sha.as_deref(), a.size), (Some("sha2"), 9));

    // One node holds one path.
    let twice = db
        .upsert_source_items(
            f.library,
            &[SourceItem {
                node: file.into(),
                path: "docs/elsewhere.txt".into(),
                repository_path: "docs/elsewhere.txt".into(),
                blob_sha: Some("sha3".into()),
                size: 1,
            }],
        )
        .await;
    assert!(twice.is_err());
    assert_eq!(db.source_items(f.library).await.unwrap().len(), 2);

    // Another path for another node is a third item; removing takes only those named.
    db.upsert_source_items(
        f.library,
        &[SourceItem {
            node: other.into(),
            path: "docs/b.txt".into(),
            repository_path: "docs/b.txt".into(),
            blob_sha: Some("sha4".into()),
            size: 2,
        }],
    )
    .await
    .unwrap();
    db.remove_source_items(f.library, &[file.into()])
        .await
        .unwrap();
    let left = db.source_items(f.library).await.unwrap();
    assert_eq!(left.len(), 2);
    assert!(left.iter().all(|i| i.path != "docs/a.txt"));
}

#[tokio::test]
async fn due_sources_returns_a_source_past_its_poll_time_and_not_one_in_backoff() {
    let f = fixture().await;
    let db = &f.test.db;
    let now = OffsetDateTime::now_utc();
    let make = |name: &'static str| {
        let f = &f;
        async move {
            let library = put(
                &f.test,
                new_id(),
                NodeKind::Library,
                Some(f.site),
                f.site,
                name,
            )
            .await;
            f.test
                .db
                .connect_source(&new_source(f, library))
                .await
                .unwrap();
            library
        }
    };
    let fresh = make("fresh").await; // never checked: due
    let stale = make("stale").await; // checked long ago: due
    let recent = make("recent").await; // checked just now: not yet
    let backoff = make("backoff").await; // retry later: not due
    let past_retry = make("past_retry").await; // retry time passed: due
    let lost = make("lost").await; // not connected: not due
    let busy = make("busy").await; // syncing: not due

    let mut s = status(SourceState::Ok);
    s.last_checked_at = Some(now - Duration::seconds(3600));
    db.set_source_status(stale, &s).await.unwrap();

    s.last_checked_at = Some(now - Duration::seconds(10));
    db.set_source_status(recent, &s).await.unwrap();

    let mut s = status(SourceState::RateLimited);
    s.last_checked_at = Some(now - Duration::seconds(3600));
    s.retry_at = Some(now + Duration::seconds(1800));
    db.set_source_status(backoff, &s).await.unwrap();

    s.retry_at = Some(now - Duration::seconds(5));
    db.set_source_status(past_retry, &s).await.unwrap();

    let mut s = status(SourceState::NotConnected);
    s.last_checked_at = Some(now - Duration::seconds(3600));
    db.set_source_status(lost, &s).await.unwrap();

    let mut s = status(SourceState::Syncing);
    s.last_checked_at = Some(now - Duration::seconds(3600));
    db.set_source_status(busy, &s).await.unwrap();

    let mut due = db.due_sources(now, 100).await.unwrap();
    due.sort_by_key(|n| n.0);
    let mut expected = vec![fresh, stale, past_retry];
    expected.sort_by_key(|n| n.0);
    assert_eq!(due, expected);

    assert_eq!(db.due_sources(now, 1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn archived_or_deleted_sites_are_never_due_or_wakeable() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let now = OffsetDateTime::now_utc();
    assert_eq!(db.due_sources(now, 10).await.unwrap(), [f.library]);
    assert_eq!(
        db.wakeable_sources(now, &[f.library, f.site])
            .await
            .unwrap(),
        [f.library]
    );
    let archived = sqlx::query("update sites set archived_at = now() where id = $1")
        .bind(f.site.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    assert_eq!(archived.rows_affected(), 1);
    assert!(db.due_sources(now, 10).await.unwrap().is_empty());
    assert!(
        db.wakeable_sources(now, &[f.library])
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("update sites set archived_at = null where id = $1")
        .bind(f.site.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    // Only the site's own node is in the bin.
    sqlx::query("update nodes set deleted_at = now(), deleted_with = id where id = $1")
        .bind(f.site.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    assert!(db.due_sources(now, 10).await.unwrap().is_empty());
    assert!(
        db.wakeable_sources(now, &[f.library])
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("update nodes set deleted_at = null, deleted_with = null where id = $1")
        .bind(f.site.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    assert_eq!(db.due_sources(now, 10).await.unwrap(), [f.library]);
}

#[tokio::test]
async fn syncable_is_rechecked_from_live_state_and_retry_time() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    assert!(db.source_is_syncable(f.library, now).await.unwrap());

    let mut status = status(SourceState::Failed);
    status.retry_at = Some(now + Duration::minutes(1));
    db.set_source_status(f.library, &status).await.unwrap();
    assert!(!db.source_is_syncable(f.library, now).await.unwrap());
    assert!(
        db.source_is_syncable(f.library, now + Duration::minutes(1))
            .await
            .unwrap()
    );
    status.state = SourceState::Syncing;
    status.retry_at = None;
    db.set_source_status(f.library, &status).await.unwrap();
    assert!(!db.source_is_syncable(f.library, now).await.unwrap());
    assert!(db.source_sync_is_busy(f.library, now).await.unwrap());

    sqlx::query("update sites set archived_at = now() where id = $1")
        .bind(f.site.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    assert!(
        !db.source_is_syncable(f.library, now + Duration::minutes(1))
            .await
            .unwrap()
    );
    assert!(!db.source_sync_is_busy(f.library, now).await.unwrap());
}

#[tokio::test]
async fn wakeable_ignores_the_poll_time_but_not_a_stop_or_a_retry_time() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    // Whole microseconds, as PostgreSQL keeps them.
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    let only = [f.library];
    let wake = || db.wakeable_sources(now, &only);
    let mut s = status(SourceState::Ok);
    s.last_checked_at = Some(now);
    db.set_source_status(f.library, &s).await.unwrap();
    assert!(db.due_sources(now, 10).await.unwrap().is_empty());
    assert_eq!(wake().await.unwrap(), [f.library]);
    s.state = SourceState::Syncing;
    db.set_source_status(f.library, &s).await.unwrap();
    assert_eq!(wake().await.unwrap(), [f.library]);
    s.state = SourceState::NotConnected;
    db.set_source_status(f.library, &s).await.unwrap();
    assert!(wake().await.unwrap().is_empty());
    s.state = SourceState::Failed;
    db.set_source_status(f.library, &s).await.unwrap();
    assert_eq!(wake().await.unwrap(), [f.library]);
    // A back-off is set only on a failed connection.
    assert!(
        db.back_off_source(f.library, now + Duration::seconds(60))
            .await
            .unwrap()
    );
    assert!(wake().await.unwrap().is_empty());
    let (_, read) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(read.retry_at, Some(now + Duration::seconds(60)));
    s.state = SourceState::Ok;
    s.retry_at = None;
    db.set_source_status(f.library, &s).await.unwrap();
    assert!(
        !db.back_off_source(f.library, now + Duration::seconds(60))
            .await
            .unwrap()
    );
    assert_eq!(
        db.source_of(f.library).await.unwrap().unwrap().1.retry_at,
        None
    );
    // The half-applied marker replaces the entity tag only.
    db.mark_source_etag(f.library, "marker").await.unwrap();
    let (_, read) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(read.etag.as_deref(), Some("marker"));
    assert_eq!(read.state, SourceState::Ok);
    assert!(wake().await.unwrap().len() == 1);
    assert!(db.wakeable_sources(now, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn disconnect_returns_the_secret_ids_and_removes_the_rows_but_not_the_nodes() {
    let f = fixture().await;
    let db = &f.test.db;
    let token = db.put_secret(&sealed(b"token")).await.unwrap();
    let webhook = db.put_secret(&sealed(b"hook")).await.unwrap();
    let mut new = new_source(&f, f.library);
    new.token = Some(token);
    db.connect_source(&new).await.unwrap();
    db.change_source(
        f.library,
        &SourceChange {
            webhook_secret: Some(Some(webhook)),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let doc = document_in(&f.test, f.user, f.library, f.site, "a.txt").await;
    db.upsert_source_items(
        f.library,
        &[SourceItem {
            node: doc.into(),
            path: "a.txt".into(),
            repository_path: "a.txt".into(),
            blob_sha: Some("sha".into()),
            size: 1,
        }],
    )
    .await
    .unwrap();

    let mut secrets = db.disconnect_source(f.library).await.unwrap();
    secrets.sort_by_key(|s| s.0);
    let mut expected = vec![token, webhook];
    expected.sort_by_key(|s| s.0);
    assert_eq!(secrets, expected);

    assert!(db.source_of(f.library).await.unwrap().is_none());
    assert!(db.source_items(f.library).await.unwrap().is_empty());
    assert!(db.node(doc.into()).await.unwrap().is_some());
    assert!(db.node(f.library).await.unwrap().is_some());
    // The caller deletes the secrets once the connection is gone.
    for id in secrets {
        db.delete_secret(id).await.unwrap();
    }
    assert!(db.disconnect_source(f.library).await.unwrap().is_empty());
}

#[tokio::test]
async fn poll_seconds_below_60_is_refused() {
    let f = fixture().await;
    let db = &f.test.db;
    let mut new = new_source(&f, f.library);
    new.poll_seconds = 59;
    assert!(db.connect_source(&new).await.is_err());
    new.poll_seconds = 86_401;
    assert!(db.connect_source(&new).await.is_err());
    assert!(db.source_of(f.library).await.unwrap().is_none());

    new.poll_seconds = 60;
    db.connect_source(&new).await.unwrap();
    let result = db
        .change_source(
            f.library,
            &SourceChange {
                poll_seconds: Some(30),
                ..SourceChange::default()
            },
        )
        .await;
    assert!(result.is_err());
    let (config, _) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.poll_seconds, 60);
}

#[tokio::test]
async fn a_long_error_is_cut_to_what_the_column_holds() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let mut s = status(SourceState::Failed);
    s.last_error = Some("x".repeat(1000));
    db.set_source_status(f.library, &s).await.unwrap();
    let (_, back) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(back.last_error.unwrap().chars().count(), 300);
}

#[tokio::test]
async fn a_secret_row_holds_only_ciphertext() {
    let f = fixture().await;
    let plaintext = "ghp_this_is_a_plaintext_token";
    // Stands in for what `seal` produces: bytes that are not the plaintext.
    let ciphertext: Vec<u8> = plaintext.bytes().map(|b| b ^ 0x5a).collect();
    let id = f
        .test
        .db
        .put_secret(&SealedRow {
            key_id: 3,
            nonce: [9; 12],
            ciphertext,
        })
        .await
        .unwrap();
    let row = sqlx::query("select secrets::text as everything from secrets where id = $1")
        .bind(id.0)
        .fetch_one(f.test.db.test_pool())
        .await
        .unwrap();
    let everything: String = row.get("everything");
    assert!(!everything.contains(plaintext), "{everything}");
    assert!(!everything.contains("ghp_"), "{everything}");
}

#[tokio::test]
async fn the_source_limits_are_settings_only_administrators_change() {
    let f = fixture().await;
    let db = &f.test.db;
    db.set_setting(f.user, "source_max_files", serde_json::json!(1000))
        .await
        .unwrap();
    db.set_setting(f.user, "source_max_bytes", serde_json::json!(1_048_576))
        .await
        .unwrap();
    assert_eq!(
        db.setting("source_max_files").await.unwrap(),
        Some(serde_json::json!(1000))
    );
    for bad in [
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!("many"),
    ] {
        assert_eq!(
            db.set_setting(f.user, "source_max_files", bad).await,
            Err(AccessError::InvalidSetting)
        );
    }
    assert_eq!(
        db.set_setting(f.user, "source_max_bytes", serde_json::json!(0))
            .await,
        Err(AccessError::InvalidSetting)
    );
}

#[tokio::test]
async fn change_source_answers_the_secrets_it_displaces_and_moves_the_generation() {
    let f = fixture().await;
    let db = &f.test.db;
    let first = db.put_secret(&sealed(b"one")).await.unwrap();
    let mut new = new_source(&f, f.library);
    new.token = Some(first);
    db.connect_source(&new).await.unwrap();
    assert_eq!(db.source_generation(f.library).await.unwrap(), Some(0));

    // Nothing of a secret changes: nothing displaced, and the generation still moves.
    let displaced = db
        .change_source(
            f.library,
            &SourceChange {
                poll_seconds: Some(600),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    assert!(displaced.is_empty());
    assert_eq!(db.source_generation(f.library).await.unwrap(), Some(1));

    let second = db.put_secret(&sealed(b"two")).await.unwrap();
    let hook = db.put_secret(&sealed(b"hook")).await.unwrap();
    let displaced = db
        .change_source(
            f.library,
            &SourceChange {
                token: Some(Some(second)),
                webhook_secret: Some(Some(hook)),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(displaced, vec![first]);
    let displaced = db
        .change_source(
            f.library,
            &SourceChange {
                token: Some(None),
                webhook_secret: Some(None),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(displaced, vec![second, hook]);
    assert_eq!(db.source_generation(f.library).await.unwrap(), Some(3));
    assert!(db.bump_source_generation(f.library).await.unwrap());
    assert_eq!(db.source_generation(f.library).await.unwrap(), Some(4));
    assert_eq!(db.source_generation(f.site).await.unwrap(), None);
}

#[tokio::test]
async fn a_sync_of_an_old_generation_records_nothing_but_a_requeue() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let now = OffsetDateTime::now_utc();
    assert!(db.mark_source_syncing(f.library, now, 0).await.unwrap());
    let (_, s) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(s.state, SourceState::Syncing);

    db.change_source(
        f.library,
        &SourceChange {
            folder: Some("docs".into()),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    let mut done = status(SourceState::Ok);
    done.last_commit = Some("abc".into());
    done.etag = Some("\"e\"".into());
    assert!(!db.set_source_status_if(f.library, &done, 0).await.unwrap());
    assert!(!db.mark_source_syncing(f.library, now, 0).await.unwrap());
    db.requeue_source(f.library, Some("dirty")).await.unwrap();
    let (_, s) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(s.state, SourceState::Connecting);
    assert_eq!(s.last_commit, None);
    assert_eq!(s.etag.as_deref(), Some("dirty"));
    assert_eq!(s.last_checked_at, None);
    assert!(db.due_sources(now, 10).await.unwrap().contains(&f.library));

    assert!(db.set_source_status_if(f.library, &done, 1).await.unwrap());
    let (_, s) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(s.last_commit.as_deref(), Some("abc"));
}

#[tokio::test]
async fn stale_sync_recovery_skips_a_source_whose_advisory_lock_is_held() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let now = OffsetDateTime::now_utc();
    let before = now - Duration::minutes(15);
    let mut syncing = status(SourceState::Syncing);
    syncing.last_checked_at = Some(now - Duration::minutes(20));
    db.set_source_status(f.library, &syncing).await.unwrap();

    let source_lock = db.try_lock_source(f.library).await.unwrap().unwrap();
    assert_eq!(db.reset_stale_syncs(before).await.unwrap(), 0);
    let (_, still_running) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(still_running.state, SourceState::Syncing);

    drop(source_lock);
    assert_eq!(db.reset_stale_syncs(before).await.unwrap(), 1);
    let (_, recovered) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(recovered.state, SourceState::Connecting);
}

#[tokio::test]
async fn a_rate_limit_survives_a_generation_change_and_is_never_shortened() {
    let f = fixture().await;
    let db = &f.test.db;
    db.connect_source(&new_source(&f, f.library)).await.unwrap();
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    let later = now + Duration::hours(1);

    assert!(db.bump_source_generation(f.library).await.unwrap());
    db.requeue_source(f.library, None).await.unwrap();
    db.rate_limit_source(f.library, later, "GitHub rate limit", None)
        .await
        .unwrap();
    db.rate_limit_source(
        f.library,
        now + Duration::minutes(5),
        "shorter",
        Some("dirty"),
    )
    .await
    .unwrap();

    let (_, status) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(status.state, SourceState::RateLimited);
    assert_eq!(status.retry_at, Some(later));
    assert_eq!(status.last_error.as_deref(), Some("shorter"));
    assert_eq!(status.etag.as_deref(), Some("dirty"));
    assert!(
        db.due_sources(later - Duration::seconds(1), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn populated_schema_migrates_from_live_log_to_github_sources() {
    let test = TestDb::create_at(Some(18)).await;
    let ann = user(&test, "MigrationAnn", true).await;
    let site = site(&test, "Migration Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Migration Library",
    )
    .await;
    test.db.migrate_to(19).await.unwrap();
    let token = test
        .db
        .put_secret(&sealed(b"migration cipher"))
        .await
        .unwrap();
    let document = document_in(&test, ann.id, library, site, "A.md").await;
    sqlx::query(
        "insert into library_sources \
         (library_id, kind, owner, repo, branch, folder, token_secret, poll_seconds, \
          connected_by, generation) \
         values ($1, 'github', 'migration-owner', 'migration-repo', 'main', '', $2, 300, $3, 7)",
    )
    .bind(library.0)
    .bind(token.0)
    .bind(ann.id.0)
    .execute(test.db.test_pool())
    .await
    .unwrap();
    sqlx::query(
        "insert into source_items \
         (library_id, path, repository_path, node_id, blob_sha, size) \
         values ($1, 'A.md', 'docs/A.md', $2, 'blob-one', 7)",
    )
    .bind(library.0)
    .bind(NodeId::from(document).0)
    .execute(test.db.test_pool())
    .await
    .unwrap();

    let generation: i64 =
        sqlx::query_scalar("select generation from library_sources where library_id = $1")
            .bind(library.0)
            .fetch_one(test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(generation, 7);

    let row = sqlx::query(
        "select path, repository_path, blob_sha, size from source_items where library_id = $1",
    )
    .bind(library.0)
    .fetch_one(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("path"), "A.md");
    assert_eq!(row.get::<String, _>("repository_path"), "docs/A.md");
    assert_eq!(
        row.get::<Option<String>, _>("blob_sha").as_deref(),
        Some("blob-one")
    );
    assert_eq!(row.get::<i64, _>("size"), 7);
    // Read as the schema of this version holds it: the provider column arrives in 0029.
    let cipher: Vec<u8> = sqlx::query_scalar("select ciphertext from secrets where id = $1")
        .bind(token.0)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
    assert_eq!(cipher, sealed(b"migration cipher").ciphertext);

    // The file-share columns arrive in 0022 (after G4's 0020 and 0021).
    test.db.migrate_to(22).await.unwrap();

    let migrated =
        sqlx::query("select location, mode, generation from library_sources where library_id = $1")
            .bind(library.0)
            .fetch_one(test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(migrated.get::<Option<String>, _>("location"), None);
    assert_eq!(migrated.get::<String, _>("mode"), "read_only");
    assert_eq!(migrated.get::<i64, _>("generation"), 7);
    let migrated_items = test.db.source_items(library).await.unwrap();
    assert_eq!(migrated_items.len(), 1);
    assert_eq!(migrated_items[0].repository_path, "docs/A.md");
    assert_eq!(migrated_items[0].blob_sha.as_deref(), Some("blob-one"));

    let current_library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Current Library",
    )
    .await;
    let current = NewSource {
        library: current_library,
        kind: SourceKind::Github,
        owner: "current-owner".into(),
        repo: "current-repo".into(),
        branch: "main".into(),
        folder: String::new(),
        location: None,
        mode: SourceMode::ReadOnly,
        token: None,
        poll_seconds: 300,
        connected_by: ann.id,
    };
    test.db.connect_source(&current).await.unwrap();
    assert_eq!(
        test.db.source_of(current_library).await.unwrap().unwrap().0,
        SourceConfig {
            library: current_library,
            kind: SourceKind::Github,
            owner: "current-owner".into(),
            repo: "current-repo".into(),
            branch: "main".into(),
            folder: String::new(),
            location: None,
            mode: SourceMode::ReadOnly,
            token: None,
            webhook_secret: None,
            poll_seconds: 300,
            connected_by: ann.id,
        }
    );

    test.db
        .change_source(
            library,
            &SourceChange {
                poll_seconds: Some(600),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        test.db
            .source_of(library)
            .await
            .unwrap()
            .unwrap()
            .0
            .poll_seconds,
        600
    );
    assert_eq!(test.db.source_items(library).await.unwrap().len(), 1);
}
