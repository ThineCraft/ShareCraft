//! The live session log: sessions, places, entries and each client's numbers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{ClientId, DocId, Seq, UserId};
use common::{tree, user};
use records::test_support::TestDb;
use records::{GoneReason, LiveEntryRow, LiveLocal, NewLiveChange};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const FORMAT: &str = "stand-in/1";

/// A fixed instant with microseconds, which PostgreSQL keeps exactly.
fn at(step: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
        + Duration::microseconds(1_000_123 * step)
}

struct Setup {
    test: TestDb,
    doc: DocId,
    ann: UserId,
    bob: UserId,
}

async fn setup() -> Setup {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await.id;
    let bob = user(&test, "Bob", false).await.id;
    let doc = tree(&test, ann).await.doc;
    Setup {
        test,
        doc,
        ann,
        bob,
    }
}

fn client() -> ClientId {
    ClientId(Uuid::new_v4())
}

/// A change at `seq` from `client`'s `local`, with fields that differ from change to change.
fn change<'a>(
    seq: u64,
    session: Uuid,
    client: ClientId,
    user: UserId,
    local: u64,
    payload: &'a [u8],
    witnesses: &'a [u32],
) -> NewLiveChange<'a> {
    NewLiveChange {
        seq: Seq(seq),
        session,
        client,
        local,
        participant: 1,
        user,
        seen: Seq(seq - 1),
        at: at(i64::try_from(seq).unwrap()),
        seed: u32::MAX - u32::try_from(seq).unwrap(),
        format: FORMAT,
        payload,
        witnesses,
    }
}

async fn session(s: &Setup, step: i64) -> Uuid {
    let id = Uuid::new_v4();
    s.test
        .db
        .start_live_session(s.doc, id, FORMAT, at(step))
        .await
        .unwrap();
    id
}

async fn head(s: &Setup) -> Seq {
    s.test.db.load_live(s.doc).await.unwrap().head
}

#[tokio::test]
async fn empty_document_loads_empty() {
    let s = setup().await;
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.head, Seq(0));
    assert_eq!(loaded.last_participant, 0);
    assert_eq!(loaded.format, None);
    assert_eq!(loaded.verified_through, Seq(0));
    assert!(loaded.entries.is_empty());
    assert!(loaded.open_session.is_none());
    assert!(loaded.clients.is_empty());
}

#[tokio::test]
async fn log_reloads_exactly() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    assert_eq!(
        s.test
            .db
            .add_live_participant(s.doc, session, one, s.ann, at(0))
            .await
            .unwrap(),
        1
    );
    let payloads: [&[u8]; 3] = [
        b"{\"set\":\"a\"}",
        "nul \u{0} and \u{1F600}".as_bytes(),
        b"",
    ];
    let witnesses: [&[u32]; 3] = [&[1], &[1, 2], &[]];
    for (i, (payload, witnesses)) in payloads.iter().zip(witnesses).enumerate() {
        let n = u64::try_from(i).unwrap() + 1;
        s.test
            .db
            .append_live_change(
                s.doc,
                &change(n, session, one, s.ann, n, payload, witnesses),
            )
            .await
            .unwrap();
    }

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.head, Seq(3));
    assert_eq!(loaded.format.as_deref(), Some(FORMAT));
    assert_eq!(loaded.clients, vec![(one, s.ann, 4)]);
    assert_eq!(loaded.entries.len(), 3);
    for (i, entry) in loaded.entries.iter().enumerate() {
        let n = u64::try_from(i).unwrap() + 1;
        let want = change(n, session, one, s.ann, n, payloads[i], witnesses[i]);
        assert_eq!(
            entry,
            &LiveEntryRow::Change {
                seq: want.seq,
                session,
                client: one,
                local: n,
                participant: 1,
                user: s.ann,
                seen: want.seen,
                at: want.at,
                seed: want.seed,
                format: FORMAT.to_string(),
                payload: payloads[i].to_vec(),
                witnesses: witnesses[i].to_vec(),
                cancelled_by: None,
            }
        );
    }
    let open = loaded.open_session.unwrap();
    assert_eq!(open.id, session);
    assert_eq!(open.format, FORMAT);
    assert_eq!(open.first_seq, Seq(1));
    assert_eq!(open.started_at, at(0));
    assert_eq!(open.participants.len(), 1);
    assert_eq!(open.participants[0].number, 1);
    assert_eq!(open.participants[0].client, one);
    assert_eq!(open.participants[0].user, s.ann);
    assert_eq!(open.participants[0].joined_at, at(0));
    assert_eq!(open.participants[0].gone, None);
}

#[tokio::test]
async fn cancel_rows_load_back() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[1]))
        .await
        .unwrap();
    // Task 10 writes cancels; written here directly to check that they load.
    let pool = s.test.db.test_pool();
    sqlx::query(
        "insert into live_entries (document_id, seq, session_id, kind, at, seed, format, \
                                   cancels, reports) \
         values ($1, 2, $2, 'cancel', $3, 7, $4, 1, 2)",
    )
    .bind(s.doc.0)
    .bind(session)
    .bind(at(2))
    .bind(FORMAT)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "update live_entries set cancelled_by = 2, payload = '' \
         where document_id = $1 and seq = 1",
    )
    .bind(s.doc.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("update live_logs set head = 2 where document_id = $1")
        .bind(s.doc.0)
        .execute(pool)
        .await
        .unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert!(matches!(
        &loaded.entries[0],
        LiveEntryRow::Change { cancelled_by: Some(Seq(2)), payload, .. } if payload.is_empty()
    ));
    assert_eq!(
        loaded.entries[1],
        LiveEntryRow::Cancel {
            seq: Seq(2),
            session,
            at: at(2),
            seed: 7,
            cancels: Seq(1),
            reports: 2,
        }
    );
}

#[tokio::test]
async fn head_never_skips_or_repeats() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    assert!(
        s.test
            .db
            .append_live_change(s.doc, &change(2, session, one, s.ann, 1, b"x", &[]))
            .await
            .is_err()
    );
    assert_eq!(head(&s).await, Seq(0));
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[]))
        .await
        .unwrap();
    assert_eq!(head(&s).await, Seq(1));
    let other = client();
    assert!(
        s.test
            .db
            .append_live_change(s.doc, &change(1, session, other, s.bob, 1, b"y", &[]))
            .await
            .is_err()
    );
    assert_eq!(head(&s).await, Seq(1));
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.clients, vec![(one, s.ann, 2)]);
}

#[tokio::test]
async fn same_client_local_cannot_be_stored_twice() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[]))
        .await
        .unwrap();
    assert!(
        s.test
            .db
            .append_live_change(s.doc, &change(2, session, one, s.ann, 1, b"x", &[]))
            .await
            .is_err()
    );
    assert_eq!(head(&s).await, Seq(1));
}

#[tokio::test]
async fn participant_numbers_are_never_reused() {
    let s = setup().await;
    let first = session(&s, 0).await;
    let (one, two) = (client(), client());
    let db = &s.test.db;
    assert_eq!(
        db.add_live_participant(s.doc, first, one, s.ann, at(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db.add_live_participant(s.doc, first, two, s.bob, at(2))
            .await
            .unwrap(),
        2
    );
    db.end_live_session(first, at(3)).await.unwrap();
    let second = session(&s, 4).await;
    assert_eq!(
        db.add_live_participant(s.doc, second, one, s.ann, at(5))
            .await
            .unwrap(),
        3
    );
    assert_eq!(db.load_live(s.doc).await.unwrap().last_participant, 3);
}

#[tokio::test]
async fn participant_numbers_start_at_one() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let refused = sqlx::query(
        "insert into live_participants (session_id, number, client_id, user_id, joined_at) \
         values ($1, 0, $2, $3, now())",
    )
    .bind(session)
    .bind(Uuid::new_v4())
    .bind(s.ann.0)
    .execute(s.test.db.test_pool())
    .await;
    assert!(refused.is_err(), "a place numbered 0 was stored");
    let mut change = change(1, session, client(), s.ann, 1, b"x", &[]);
    change.participant = 0;
    assert!(s.test.db.append_live_change(s.doc, &change).await.is_err());
    assert_eq!(head(&s).await, Seq(0));
}

#[tokio::test]
async fn places_join_only_an_open_session_of_their_document() {
    let s = setup().await;
    let first = session(&s, 0).await;
    let db = &s.test.db;
    db.end_live_session(first, at(1)).await.unwrap();
    assert!(
        db.add_live_participant(s.doc, first, client(), s.ann, at(2))
            .await
            .is_err()
    );
    let other = tree(&s.test, s.bob).await.doc;
    let second = session(&s, 3).await;
    assert!(
        db.add_live_participant(other, second, client(), s.bob, at(4))
            .await
            .is_err()
    );
    assert_eq!(db.load_live(s.doc).await.unwrap().last_participant, 0);
}

#[tokio::test]
async fn one_open_session_per_document() {
    let s = setup().await;
    session(&s, 0).await;
    assert!(
        s.test
            .db
            .start_live_session(s.doc, Uuid::new_v4(), FORMAT, at(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn places_and_sessions_end() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let db = &s.test.db;
    let one = db
        .add_live_participant(s.doc, session, client(), s.ann, at(1))
        .await
        .unwrap();
    let two = db
        .add_live_participant(s.doc, session, client(), s.bob, at(1))
        .await
        .unwrap();
    db.end_live_place(session, one, GoneReason::Expired, at(2))
        .await
        .unwrap();
    // A place already ended keeps its first reason.
    db.end_live_place(session, one, GoneReason::Left, at(3))
        .await
        .unwrap();
    let open = db.load_live(s.doc).await.unwrap().open_session.unwrap();
    let gone: Vec<_> = open
        .participants
        .iter()
        .map(|p| (p.number, p.gone))
        .collect();
    assert_eq!(gone, vec![(one, Some(GoneReason::Expired)), (two, None)]);

    for reason in [GoneReason::Removed, GoneReason::CannotOpen] {
        let n = db
            .add_live_participant(s.doc, session, client(), s.bob, at(4))
            .await
            .unwrap();
        db.end_live_place(session, n, reason, at(5)).await.unwrap();
        let open = db.load_live(s.doc).await.unwrap().open_session.unwrap();
        assert_eq!(
            open.participants
                .iter()
                .find(|p| p.number == n)
                .unwrap()
                .gone,
            Some(reason)
        );
    }

    db.end_live_session(session, at(6)).await.unwrap();
    assert!(db.load_live(s.doc).await.unwrap().open_session.is_none());
    // A new session may start once the old one ended.
    db.start_live_session(s.doc, Uuid::new_v4(), FORMAT, at(7))
        .await
        .unwrap();
}

#[tokio::test]
async fn sessions_start_after_the_head() {
    let s = setup().await;
    let first = session(&s, 0).await;
    let one = client();
    for n in 1..=2 {
        s.test
            .db
            .append_live_change(s.doc, &change(n, first, one, s.ann, n, b"x", &[]))
            .await
            .unwrap();
    }
    s.test.db.end_live_session(first, at(3)).await.unwrap();
    session(&s, 4).await;
    let open = s
        .test
        .db
        .load_live(s.doc)
        .await
        .unwrap()
        .open_session
        .unwrap();
    assert_eq!(open.first_seq, Seq(3));
}

#[tokio::test]
async fn consumed_local_moves_next_local() {
    let s = setup().await;
    let one = client();
    s.test
        .db
        .consume_live_local(s.doc, one, s.ann, 1, "read_only", at(1))
        .await
        .unwrap();
    assert_eq!(
        s.test.db.load_live(s.doc).await.unwrap().clients,
        vec![(one, s.ann, 2)]
    );
    assert_eq!(head(&s).await, Seq(0));
}

#[tokio::test]
async fn next_local_never_goes_back() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    let db = &s.test.db;
    // Local numbers may skip: 5 is stored, then a refusal and a change for lower numbers.
    db.append_live_change(s.doc, &change(1, session, one, s.ann, 5, b"x", &[]))
        .await
        .unwrap();
    db.consume_live_local(s.doc, one, s.ann, 2, "too_large", at(2))
        .await
        .unwrap();
    db.append_live_change(s.doc, &change(2, session, one, s.ann, 3, b"y", &[]))
        .await
        .unwrap();
    assert_eq!(
        db.load_live(s.doc).await.unwrap().clients,
        vec![(one, s.ann, 6)]
    );
}

#[tokio::test]
async fn each_local_is_stored_refused_or_unseen() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let (one, two) = (client(), client());
    let db = &s.test.db;
    db.append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[]))
        .await
        .unwrap();
    db.consume_live_local(s.doc, one, s.ann, 2, "wrong_format", at(2))
        .await
        .unwrap();
    db.append_live_change(s.doc, &change(2, session, one, s.ann, 4, b"y", &[]))
        .await
        .unwrap();

    // A restarted room reads the same answers.
    let again = s.test.connect_again().await;
    for db in [db, &again] {
        assert_eq!(
            db.live_local(s.doc, one, 1).await.unwrap(),
            LiveLocal::Stored { seq: Seq(1) }
        );
        assert_eq!(
            db.live_local(s.doc, one, 2).await.unwrap(),
            LiveLocal::Refused {
                reason: "wrong_format".to_string()
            }
        );
        assert_eq!(
            db.live_local(s.doc, one, 3).await.unwrap(),
            LiveLocal::Unseen
        );
        assert_eq!(
            db.live_local(s.doc, one, 4).await.unwrap(),
            LiveLocal::Stored { seq: Seq(2) }
        );
        assert_eq!(
            db.live_local(s.doc, one, 5).await.unwrap(),
            LiveLocal::Unseen
        );
        assert_eq!(
            db.live_local(s.doc, two, 1).await.unwrap(),
            LiveLocal::Unseen
        );
    }

    // Refusing again keeps the first reason; a refused number is never stored, and a stored
    // one is never refused.
    db.consume_live_local(s.doc, one, s.ann, 2, "rate_limited", at(3))
        .await
        .unwrap();
    assert_eq!(
        db.live_local(s.doc, one, 2).await.unwrap(),
        LiveLocal::Refused {
            reason: "wrong_format".to_string()
        }
    );
    assert!(
        db.append_live_change(s.doc, &change(3, session, one, s.ann, 2, b"z", &[]))
            .await
            .is_err()
    );
    assert_eq!(head(&s).await, Seq(2));
    assert!(
        db.consume_live_local(s.doc, one, s.ann, 1, "too_large", at(4))
            .await
            .is_err()
    );
    assert_eq!(
        db.live_local(s.doc, one, 1).await.unwrap(),
        LiveLocal::Stored { seq: Seq(1) }
    );
}

#[tokio::test]
async fn a_client_belongs_to_one_person() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    let db = &s.test.db;
    db.append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[]))
        .await
        .unwrap();
    assert!(
        db.append_live_change(s.doc, &change(2, session, one, s.bob, 2, b"y", &[]))
            .await
            .is_err()
    );
    assert!(
        db.consume_live_local(s.doc, one, s.bob, 2, "read_only", at(2))
            .await
            .is_err()
    );
    assert_eq!(head(&s).await, Seq(1));
    assert_eq!(
        db.load_live(s.doc).await.unwrap().clients,
        vec![(one, s.ann, 2)]
    );
}

#[tokio::test]
async fn migration_0018_upgrades_an_existing_database() {
    // 0016 is the latest migration before this one on this branch; 0017 is taken by another
    // branch and is not here.
    let test = TestDb::create_at(Some(16)).await;
    let ann = user(&test, "Ann", false).await.id;
    let doc = tree(&test, ann).await.doc;
    test.db.migrate().await.unwrap();
    let loaded = test.db.load_live(doc).await.unwrap();
    assert_eq!(loaded.head, Seq(0));
    let session = Uuid::new_v4();
    test.db
        .start_live_session(doc, session, FORMAT, at(0))
        .await
        .unwrap();
    test.db
        .append_live_change(doc, &change(1, session, client(), ann, 1, b"x", &[]))
        .await
        .unwrap();
    assert_eq!(test.db.load_live(doc).await.unwrap().head, Seq(1));
}
