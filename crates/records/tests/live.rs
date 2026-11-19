//! The live session log: sessions, places, entries and each client's numbers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{ClientId, DocId, Seq, UserId};
use common::{Gate, tree, user, wait_blocked};
use records::test_support::TestDb;
use records::{
    GoneReason, LiveCheckpointRow, LiveCheckpointWriter, LiveEntryRow, LiveLocal,
    LiveReturnTombstoneRow, LiveSessionDataRow, NewLiveCancel, NewLiveChange, NewLiveCheckpoint,
    RevisionRecord,
};
use serde_json::json;
use sqlx::Connection;
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

async fn live_log_gate(s: &Setup) -> Gate {
    let mut conn = sqlx::PgConnection::connect(&s.test.url()).await.unwrap();
    let pid = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    sqlx::query("begin").execute(&mut conn).await.unwrap();
    sqlx::query("select document_id from live_logs where document_id = $1 for update")
        .bind(s.doc.0)
        .execute(&mut conn)
        .await
        .unwrap();
    Gate { conn, pid }
}

async fn revision(s: &Setup, id: char, parent: Option<&str>) -> String {
    let revision = id.to_string().repeat(40);
    assert!(
        s.test
            .db
            .append_revision(
                parent,
                &RevisionRecord {
                    doc: s.doc,
                    revision: revision.clone(),
                    committed_at: at(i64::from(id as u8)),
                    authors: vec![(s.ann.0.to_string(), "Ann".to_string())],
                    name: "live.grid".to_string(),
                    fields: json!({}),
                    size: 10,
                },
            )
            .await
            .unwrap()
    );
    revision
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
    assert!(loaded.participants.is_empty());
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
async fn cancel_live_entry_is_atomic() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"crash", &[1, 2]))
        .await
        .unwrap();

    s.test
        .db
        .cancel_live_entry(
            s.doc,
            &NewLiveCancel {
                seq: Seq(2),
                session,
                cancels: Seq(1),
                reports: 2,
                at: at(2),
                seed: 17,
            },
        )
        .await
        .unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.head, Seq(2));
    assert!(matches!(
        &loaded.entries[0],
        LiveEntryRow::Change {
            cancelled_by: Some(Seq(2)),
            payload,
            ..
        } if payload.is_empty()
    ));
    assert!(matches!(
        loaded.entries[1],
        LiveEntryRow::Cancel {
            seq: Seq(2),
            cancels: Seq(1),
            reports: 2,
            seed: 17,
            ..
        }
    ));

    assert!(
        s.test
            .db
            .cancel_live_entry(
                s.doc,
                &NewLiveCancel {
                    seq: Seq(3),
                    session,
                    cancels: Seq(1),
                    reports: 1,
                    at: at(3),
                    seed: 18,
                },
            )
            .await
            .is_err()
    );
    assert_eq!(head(&s).await, Seq(2));
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
async fn load_keeps_participants_from_ended_sessions() {
    let s = setup().await;
    let first = session(&s, 0).await;
    let old_client = client();
    let old_number = s
        .test
        .db
        .add_live_participant(s.doc, first, old_client, s.ann, at(1))
        .await
        .unwrap();
    s.test
        .db
        .end_live_place(first, old_number, GoneReason::Left, at(2))
        .await
        .unwrap();
    s.test.db.end_live_session(first, at(3)).await.unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert!(loaded.open_session.is_none());
    assert!(matches!(
        loaded.participants.as_slice(),
        [place]
            if place.number == old_number
                && place.client == old_client
                && place.user == s.ann
                && place.gone == Some(GoneReason::Left)
    ));
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
    // Local numbers may skip forwards, but an unseen number behind that skip is out of order.
    db.append_live_change(s.doc, &change(1, session, one, s.ann, 5, b"x", &[]))
        .await
        .unwrap();
    assert!(
        db.consume_live_local(s.doc, one, s.ann, 2, "too_large", at(2))
            .await
            .is_err()
    );
    assert!(
        db.append_live_change(s.doc, &change(2, session, one, s.ann, 3, b"y", &[]))
            .await
            .is_err()
    );
    assert_eq!(
        db.load_live(s.doc).await.unwrap().clients,
        vec![(one, s.ann, 6)]
    );
    assert_eq!(head(&s).await, Seq(1));
    assert_eq!(
        db.live_local(s.doc, one, 2).await.unwrap(),
        LiveLocal::Unseen
    );
    assert_eq!(
        db.live_local(s.doc, one, 3).await.unwrap(),
        LiveLocal::Unseen
    );
}

#[tokio::test]
async fn unseen_number_below_next_local_is_out_of_order() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    let db = &s.test.db;
    db.consume_live_local(s.doc, one, s.ann, 4, "read_only", at(1))
        .await
        .unwrap();

    let refusal = db
        .consume_live_local(s.doc, one, s.ann, 2, "wrong_format", at(2))
        .await
        .unwrap_err();
    assert!(refusal.to_string().contains("out of order"));
    let append = db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 3, b"x", &[]))
        .await
        .unwrap_err();
    assert!(append.to_string().contains("out of order"));
    assert_eq!(
        db.load_live(s.doc).await.unwrap().clients,
        vec![(one, s.ann, 5)]
    );
    assert_eq!(
        db.live_local(s.doc, one, 2).await.unwrap(),
        LiveLocal::Unseen
    );
    assert_eq!(
        db.live_local(s.doc, one, 3).await.unwrap(),
        LiveLocal::Unseen
    );
    assert_eq!(head(&s).await, Seq(0));
}

#[tokio::test]
async fn concurrent_appends_for_the_same_seq_have_one_winner() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let second = s.test.connect_again().await;

    for round in 0..8u64 {
        let seq = round + 1;
        let (one, two) = (client(), client());
        s.test
            .db
            .consume_live_local(s.doc, one, s.ann, 1, "read_only", at(1))
            .await
            .unwrap();
        second
            .consume_live_local(s.doc, two, s.bob, 1, "read_only", at(1))
            .await
            .unwrap();
        let gate = live_log_gate(&s).await;
        let first_db = s.test.db.clone();
        let second_db = second.clone();
        let doc = s.doc;
        let ann = s.ann;
        let bob = s.bob;
        let first = tokio::spawn(async move {
            first_db
                .append_live_change(doc, &change(seq, session, one, ann, 2, b"a", &[]))
                .await
        });
        let other = tokio::spawn(async move {
            second_db
                .append_live_change(doc, &change(seq, session, two, bob, 2, b"b", &[]))
                .await
        });
        wait_blocked(&s.test, gate.pid, 2).await;
        gate.open().await;

        let results = [first.await.unwrap(), other.await.unwrap()];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "round {round}: {results:?}"
        );
        let conflict = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap();
        assert!(
            conflict.to_string().contains("does not follow the head"),
            "round {round}: {conflict}"
        );
        let loaded = s.test.db.load_live(s.doc).await.unwrap();
        assert_eq!(loaded.head, Seq(seq), "round {round}");
        assert_eq!(loaded.entries.len(), usize::try_from(seq).unwrap());
        let locals: Vec<_> = loaded
            .clients
            .iter()
            .filter(|(client, _, _)| *client == one || *client == two)
            .map(|(client, _, next)| (*client, *next))
            .collect();
        assert_eq!(locals.len(), 2);
        assert_eq!(locals.iter().filter(|(_, next)| *next == 3).count(), 1);
        assert_eq!(locals.iter().filter(|(_, next)| *next == 2).count(), 1);
    }
}

#[tokio::test]
async fn concurrent_append_and_refusal_record_one_disposition() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let second = s.test.connect_again().await;

    for round in 0..8u64 {
        let seq = head(&s).await.0 + 1;
        let one = client();
        let gate = live_log_gate(&s).await;
        let append_db = s.test.db.clone();
        let refuse_db = second.clone();
        let doc = s.doc;
        let ann = s.ann;
        let append = tokio::spawn(async move {
            append_db
                .append_live_change(doc, &change(seq, session, one, ann, 1, b"x", &[]))
                .await
        });
        let refuse = tokio::spawn(async move {
            refuse_db
                .consume_live_local(doc, one, ann, 1, "read_only", at(2))
                .await
        });
        wait_blocked(&s.test, gate.pid, 2).await;
        gate.open().await;

        let results = [append.await.unwrap(), refuse.await.unwrap()];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "round {round}: {results:?}"
        );
        let disposition_count: i64 = sqlx::query_scalar(
            "select (select count(*) from live_entries \
                     where document_id = $1 and client_id = $2 and local = 1) + \
                    (select count(*) from live_refusals \
                     where document_id = $1 and client_id = $2 and local = 1)",
        )
        .bind(s.doc.0)
        .bind(one.0)
        .fetch_one(s.test.db.test_pool())
        .await
        .unwrap();
        assert_eq!(disposition_count, 1, "round {round}");
        let disposition = s.test.db.live_local(s.doc, one, 1).await.unwrap();
        match disposition {
            LiveLocal::Stored { seq: stored } => {
                assert!(results[0].is_ok(), "round {round}: {results:?}");
                assert_eq!(stored, Seq(seq));
            }
            LiveLocal::Refused { reason } => {
                assert!(results[1].is_ok(), "round {round}: {results:?}");
                assert_eq!(reason, "read_only");
            }
            LiveLocal::Unseen => panic!("round {round}: no disposition was recorded"),
        }
    }
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
async fn unverified_entries_are_kept() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    let mut old = change(1, session, one, s.ann, 1, b"old", &[]);
    old.at = at(0) - Duration::days(100);
    s.test.db.append_live_change(s.doc, &old).await.unwrap();

    let swept = s.test.db.sweep_live(at(0)).await.unwrap();
    assert_eq!(swept.entries, 0);
    assert_eq!(s.test.db.load_live(s.doc).await.unwrap().entries.len(), 1);
}

#[tokio::test]
async fn verified_entries_go_after_thirty_days() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=3 {
        s.test
            .db
            .append_live_change(
                s.doc,
                &change(seq, session, one, s.ann, seq, b"change", &[]),
            )
            .await
            .unwrap();
    }
    s.test
        .db
        .mark_live_verified(s.doc, Seq(2), at(0))
        .await
        .unwrap();
    s.test.db.end_live_session(session, at(1)).await.unwrap();

    assert_eq!(
        s.test
            .db
            .sweep_live(at(0) + Duration::days(29))
            .await
            .unwrap()
            .entries,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from live_entries where document_id = $1")
            .bind(s.doc.0)
            .fetch_one(s.test.db.test_pool())
            .await
            .unwrap(),
        3
    );

    let swept = s
        .test
        .db
        .sweep_live(at(0) + Duration::days(30) + Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(swept.entries, 2);
    let kept: Vec<i64> =
        sqlx::query_scalar("select seq from live_entries where document_id = $1 order by seq")
            .bind(s.doc.0)
            .fetch_all(s.test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(kept, [3]);
}

#[tokio::test]
async fn verified_entries_in_an_open_session_survive() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=2 {
        s.test
            .db
            .append_live_change(
                s.doc,
                &change(seq, session, one, s.ann, seq, b"change", &[]),
            )
            .await
            .unwrap();
    }
    s.test
        .db
        .mark_live_verified(s.doc, Seq(2), at(0))
        .await
        .unwrap();

    let swept = s
        .test
        .db
        .sweep_live(at(0) + Duration::days(30) + Duration::seconds(1))
        .await
        .unwrap();

    assert_eq!(swept.entries, 0);
    let kept: Vec<i64> =
        sqlx::query_scalar("select seq from live_entries where document_id = $1 order by seq")
            .bind(s.doc.0)
            .fetch_all(s.test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(kept, [1, 2]);
}

#[tokio::test]
async fn head_survives_a_full_sweep() {
    let s = setup().await;
    let first = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, first, one, s.ann, 1, b"old", &[]))
        .await
        .unwrap();
    s.test
        .db
        .mark_live_verified(s.doc, Seq(1), at(0))
        .await
        .unwrap();
    s.test.db.end_live_session(first, at(0)).await.unwrap();
    let swept = s
        .test
        .db
        .sweep_live(at(0) + Duration::days(31))
        .await
        .unwrap();

    assert_eq!(swept.entries, 1);
    assert_eq!(swept.sessions, 1);
    assert_eq!(swept.clients, 1);
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.head, Seq(1));
    assert_eq!(loaded.format, None);
    assert!(loaded.participants.is_empty());
    assert!(loaded.clients.is_empty());
    let second_format = "stand-in/2";
    let second = Uuid::new_v4();
    s.test
        .db
        .start_live_session(s.doc, second, second_format, at(1))
        .await
        .unwrap();
    let mut next = change(2, second, client(), s.bob, 1, b"new", &[]);
    next.format = second_format;
    s.test.db.append_live_change(s.doc, &next).await.unwrap();
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.head, Seq(2));
    assert_eq!(loaded.format.as_deref(), Some(second_format));
}

#[tokio::test]
async fn verified_through_never_goes_back() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=2 {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
    }
    s.test
        .db
        .mark_live_verified(s.doc, Seq(2), at(0))
        .await
        .unwrap();
    s.test
        .db
        .mark_live_verified(s.doc, Seq(1), at(10))
        .await
        .unwrap();
    s.test.db.end_live_session(session, at(11)).await.unwrap();

    assert_eq!(
        s.test.db.load_live(s.doc).await.unwrap().verified_through,
        Seq(2)
    );
    assert_eq!(
        s.test
            .db
            .sweep_live(at(0) + Duration::days(30) + Duration::seconds(1))
            .await
            .unwrap()
            .entries,
        2,
        "the backward mark did not restart the retention clock"
    );
}

#[tokio::test]
async fn old_refusals_are_swept_and_stay_out_of_order() {
    let s = setup().await;
    session(&s, 0).await;
    let one = client();
    s.test
        .db
        .consume_live_local(
            s.doc,
            one,
            s.ann,
            1,
            "read_only",
            at(0) - Duration::days(31),
        )
        .await
        .unwrap();
    s.test
        .db
        .consume_live_local(s.doc, one, s.ann, 2, "read_only", at(0))
        .await
        .unwrap();

    s.test.db.sweep_live(at(0)).await.unwrap();
    assert_eq!(
        s.test.db.live_local(s.doc, one, 1).await.unwrap(),
        LiveLocal::Unseen
    );
    let error = s
        .test
        .db
        .consume_live_local(s.doc, one, s.ann, 1, "read_only", at(1))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("out of order"), "{error}");
    assert_eq!(
        s.test.db.live_local(s.doc, one, 2).await.unwrap(),
        LiveLocal::Refused {
            reason: "read_only".to_string()
        }
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

#[tokio::test]
async fn live_checkpoint_records_revision_position_hash_and_session_data() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"entry", &[]))
        .await
        .unwrap();
    let revision = revision(&s, 'a', None).await;
    let sha256 = [7; 32];
    let session_data = br#"{"sheet":"identity"}"#;
    let writer = LiveCheckpointWriter {
        participant: 1,
        client: one,
    };

    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &revision,
                through: Seq(1),
                sha256,
                session,
                session_data,
                major_reason: Some("returning_client_before"),
                major_person: Some(s.ann),
                writer: Some(writer),
                created_at: at(2),
            },
        )
        .await
        .unwrap();
    // The same position recorded again with another writer is a conflict, not a repeat.
    assert!(
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision: &revision,
                    through: Seq(1),
                    sha256,
                    session,
                    session_data,
                    major_reason: Some("returning_client_before"),
                    major_person: Some(s.ann),
                    writer: None,
                    created_at: at(2),
                },
            )
            .await
            .is_err()
    );

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.verified_checkpoint, None);
    assert_eq!(
        loaded.unverified_checkpoints,
        [LiveCheckpointRow {
            revision,
            through: Seq(1),
            sha256,
            verified: false,
            session,
            session_data: session_data.to_vec(),
            major_reason: Some("returning_client_before".to_string()),
            major_person: Some(s.ann),
            writer: Some(writer),
        }]
    );
    assert_eq!(loaded.unsaved_entries, 1);
    assert_eq!(loaded.unsaved_bytes, 5);
}

#[tokio::test]
async fn verification_advances_only_the_matching_checkpoint() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=2 {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
    }
    let first = revision(&s, 'a', None).await;
    let second = revision(&s, 'b', Some(&first)).await;
    for (revision, through, byte) in [(&first, 1, 1), (&second, 2, 2)] {
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision,
                    through: Seq(through),
                    sha256: [byte; 32],
                    session,
                    session_data: &[byte],
                    major_reason: None,
                    major_person: None,
                    writer: None,
                    created_at: at(through as i64 + 2),
                },
            )
            .await
            .unwrap();
    }

    assert!(
        s.test
            .db
            .verify_live_checkpoint(s.doc, &first, Seq(2), at(5))
            .await
            .is_err()
    );
    assert_eq!(
        s.test.db.load_live(s.doc).await.unwrap().verified_through,
        Seq(0)
    );

    s.test
        .db
        .verify_live_checkpoint(s.doc, &first, Seq(1), at(6))
        .await
        .unwrap();
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.verified_through, Seq(1));
    assert_eq!(loaded.verified_checkpoint.unwrap().revision, first);
    assert_eq!(
        loaded
            .unverified_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.revision.as_str())
            .collect::<Vec<_>>(),
        [&second]
    );
}

#[tokio::test]
async fn checkpoint_position_cannot_exceed_head() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let revision = revision(&s, 'a', None).await;

    assert!(
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision: &revision,
                    through: Seq(1),
                    sha256: [3; 32],
                    session,
                    session_data: b"session",
                    major_reason: None,
                    major_person: None,
                    writer: None,
                    created_at: at(1),
                },
            )
            .await
            .is_err()
    );
    assert!(
        s.test
            .db
            .load_live(s.doc)
            .await
            .unwrap()
            .unverified_checkpoints
            .is_empty()
    );
}

#[tokio::test]
async fn older_checkpoint_cannot_replace_the_base() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=2 {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
    }
    let first = revision(&s, 'a', None).await;
    let second = revision(&s, 'b', Some(&first)).await;
    for (revision, through) in [(&first, 1), (&second, 2)] {
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision,
                    through: Seq(through),
                    sha256: [through as u8; 32],
                    session,
                    session_data: &[through as u8],
                    major_reason: None,
                    major_person: None,
                    writer: None,
                    created_at: at(through as i64 + 2),
                },
            )
            .await
            .unwrap();
    }
    s.test
        .db
        .verify_live_checkpoint(s.doc, &second, Seq(2), at(5))
        .await
        .unwrap();
    s.test
        .db
        .verify_live_checkpoint(s.doc, &first, Seq(1), at(20))
        .await
        .unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.verified_through, Seq(2));
    assert_eq!(loaded.verified_checkpoint.unwrap().revision, second);
    assert_eq!(
        sqlx::query_scalar::<_, OffsetDateTime>(
            "select verified_at from live_logs where document_id = $1"
        )
        .bind(s.doc.0)
        .fetch_one(s.test.db.test_pool())
        .await
        .unwrap(),
        at(5),
        "a stale verification must not restart retention"
    );
}

#[tokio::test]
async fn session_data_survives_until_covered_entries_expire() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[]))
        .await
        .unwrap();
    let revision = revision(&s, 'a', None).await;
    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &revision,
                through: Seq(1),
                sha256: [4; 32],
                session,
                session_data: b"identity table",
                major_reason: None,
                major_person: None,
                writer: None,
                created_at: at(2),
            },
        )
        .await
        .unwrap();
    s.test
        .db
        .verify_live_checkpoint(s.doc, &revision, Seq(1), at(3))
        .await
        .unwrap();
    s.test.db.end_live_session(session, at(4)).await.unwrap();

    s.test
        .db
        .sweep_live(at(3) + Duration::days(29))
        .await
        .unwrap();
    assert_eq!(
        s.test
            .db
            .load_live(s.doc)
            .await
            .unwrap()
            .verified_checkpoint
            .unwrap()
            .session_data,
        b"identity table"
    );

    s.test
        .db
        .sweep_live(at(3) + Duration::days(30) + Duration::seconds(1))
        .await
        .unwrap();
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert!(loaded.entries.is_empty());
    assert!(loaded.verified_checkpoint.unwrap().session_data.is_empty());
}

#[tokio::test]
async fn return_tombstone_survives_detail_retention() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    let participant = s
        .test
        .db
        .add_live_participant(s.doc, session, one, s.ann, at(0))
        .await
        .unwrap();
    s.test
        .db
        .append_live_change(s.doc, &change(1, session, one, s.ann, 1, b"x", &[]))
        .await
        .unwrap();
    s.test
        .db
        .mark_live_verified(s.doc, Seq(1), at(1))
        .await
        .unwrap();
    s.test.db.end_live_session(session, at(2)).await.unwrap();

    s.test
        .db
        .sweep_live(at(1) + Duration::days(31))
        .await
        .unwrap();
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert!(loaded.participants.is_empty());
    assert_eq!(
        s.test
            .db
            .live_return_tombstone(s.doc, session, participant)
            .await
            .unwrap(),
        Some(LiveReturnTombstoneRow {
            session,
            participant,
            client: one,
            user: s.ann,
            ended_at: at(2),
            final_seq: Seq(1),
        })
    );
}

#[tokio::test]
async fn revision_index_rebuild_keeps_live_checkpoint_metadata() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=2 {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
    }
    let first = revision(&s, 'a', None).await;
    let second = revision(&s, 'b', Some(&first)).await;
    for (revision, through, data) in [(&first, 1, b"first".as_slice()), (&second, 2, b"second")] {
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision,
                    through: Seq(through),
                    sha256: [through as u8; 32],
                    session,
                    session_data: data,
                    major_reason: None,
                    major_person: None,
                    writer: None,
                    created_at: at(through as i64 + 2),
                },
            )
            .await
            .unwrap();
    }
    s.test
        .db
        .verify_live_checkpoint(s.doc, &first, Seq(1), at(5))
        .await
        .unwrap();
    let history = vec![
        RevisionRecord {
            doc: s.doc,
            revision: first.clone(),
            committed_at: at(i64::from(b'a')),
            authors: vec![(s.ann.0.to_string(), "Ann".to_string())],
            name: "live.grid".to_string(),
            fields: json!({}),
            size: 10,
        },
        RevisionRecord {
            doc: s.doc,
            revision: second.clone(),
            committed_at: at(i64::from(b'b')),
            authors: vec![(s.ann.0.to_string(), "Ann".to_string())],
            name: "live.grid".to_string(),
            fields: json!({}),
            size: 10,
        },
    ];

    s.test.db.replace_revisions(s.doc, &history).await.unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.verified_checkpoint.unwrap().revision, first);
    assert_eq!(loaded.unverified_checkpoints[0].revision, second);
    assert_eq!(loaded.unverified_checkpoints[0].session_data, b"second");
}

#[tokio::test]
async fn load_keeps_retained_identity_data_for_older_sessions() {
    let s = setup().await;
    let first_session = session(&s, 0).await;
    let one = client();
    s.test
        .db
        .append_live_change(s.doc, &change(1, first_session, one, s.ann, 1, b"one", &[]))
        .await
        .unwrap();
    let first_revision = revision(&s, 'a', None).await;
    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &first_revision,
                through: Seq(1),
                sha256: [1; 32],
                session: first_session,
                session_data: b"first identities",
                major_reason: None,
                major_person: None,
                writer: None,
                created_at: at(2),
            },
        )
        .await
        .unwrap();
    s.test
        .db
        .verify_live_checkpoint(s.doc, &first_revision, Seq(1), at(3))
        .await
        .unwrap();
    s.test
        .db
        .end_live_session(first_session, at(4))
        .await
        .unwrap();

    let second_session = session(&s, 5).await;
    s.test
        .db
        .append_live_change(
            s.doc,
            &change(2, second_session, client(), s.bob, 1, b"two", &[]),
        )
        .await
        .unwrap();
    let second_revision = revision(&s, 'b', Some(&first_revision)).await;
    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &second_revision,
                through: Seq(2),
                sha256: [2; 32],
                session: second_session,
                session_data: b"second identities",
                major_reason: None,
                major_person: None,
                writer: None,
                created_at: at(6),
            },
        )
        .await
        .unwrap();
    s.test
        .db
        .verify_live_checkpoint(s.doc, &second_revision, Seq(2), at(7))
        .await
        .unwrap();

    assert_eq!(
        s.test
            .db
            .load_live(s.doc)
            .await
            .unwrap()
            .retained_session_data,
        [
            LiveSessionDataRow {
                session: first_session,
                through: Seq(1),
                session_data: b"first identities".to_vec(),
            },
            LiveSessionDataRow {
                session: second_session,
                through: Seq(2),
                session_data: b"second identities".to_vec(),
            },
        ]
    );
}

#[tokio::test]
async fn zero_position_checkpoint_survives_sweep_and_loads_for_verification() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let revision = revision(&s, 'a', None).await;
    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &revision,
                through: Seq(0),
                sha256: [9; 32],
                session,
                session_data: b"empty workbook identities",
                major_reason: None,
                major_person: None,
                writer: None,
                created_at: at(1),
            },
        )
        .await
        .unwrap();

    s.test.db.sweep_live(at(2)).await.unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.unverified_checkpoints.len(), 1);
    assert_eq!(loaded.unverified_checkpoints[0].revision, revision);
    assert_eq!(
        loaded.unverified_checkpoints[0].session_data,
        b"empty workbook identities"
    );
}

#[tokio::test]
async fn unverified_zero_checkpoint_keeps_session_data_after_its_session_ends() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let revision = revision(&s, 'a', None).await;
    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &revision,
                through: Seq(0),
                sha256: [9; 32],
                session,
                session_data: b"empty workbook identities",
                major_reason: None,
                major_person: None,
                writer: None,
                created_at: at(1),
            },
        )
        .await
        .unwrap();
    s.test.db.end_live_session(session, at(2)).await.unwrap();

    s.test
        .db
        .sweep_live(at(2) + Duration::days(31))
        .await
        .unwrap();

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.unverified_checkpoints.len(), 1);
    assert_eq!(
        loaded.unverified_checkpoints[0].session_data, b"empty workbook identities",
        "an unverified checkpoint can still be verified, so it keeps its identity data"
    );
}

#[tokio::test]
async fn return_tombstone_lookup_is_by_place_within_its_document() {
    let s = setup().await;
    let other = tree(&s.test, s.ann).await.doc;
    let session = session(&s, 0).await;
    let one = client();
    let participant = s
        .test
        .db
        .add_live_participant(s.doc, session, one, s.ann, at(0))
        .await
        .unwrap();
    s.test.db.end_live_session(session, at(1)).await.unwrap();

    let found = s
        .test
        .db
        .live_return_tombstone(s.doc, session, participant)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.client, one);
    assert_eq!(found.final_seq, Seq(0));
    assert_eq!(
        s.test
            .db
            .live_return_tombstone(other, session, participant)
            .await
            .unwrap(),
        None,
        "a place of another document is not this document's tombstone"
    );
    assert_eq!(
        s.test
            .db
            .live_return_tombstone(s.doc, session, participant + 1)
            .await
            .unwrap(),
        None
    );
}

/// The Git description of `revision` as the revision index stores it.
fn revision_record(s: &Setup, revision: &str) -> RevisionRecord {
    RevisionRecord {
        doc: s.doc,
        revision: revision.to_owned(),
        committed_at: at(5),
        authors: vec![(s.ann.0.to_string(), "Ann".to_string())],
        name: "live.grid".to_string(),
        fields: json!({ "verified": false, "live_through": 0 }),
        size: 10,
    }
}

fn written_checkpoint<'a>(written: &'a str, session: Uuid) -> NewLiveCheckpoint<'a> {
    NewLiveCheckpoint {
        revision: written,
        through: Seq(0),
        sha256: [4; 32],
        session,
        session_data: b"identities",
        major_reason: None,
        major_person: None,
        writer: None,
        created_at: at(6),
    }
}

#[tokio::test]
async fn checkpoint_records_a_missing_revision_row_with_its_metadata() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let base = revision(&s, 'a', None).await;
    let written = "b".repeat(40);
    let checkpoint = written_checkpoint(&written, session);
    assert!(
        s.test
            .db
            .record_live_checkpoint(s.doc, &checkpoint)
            .await
            .is_err(),
        "the plain record still needs the revision row"
    );

    s.test
        .db
        .record_live_checkpoint_revision(Some(&base), &revision_record(&s, &written), &checkpoint)
        .await
        .unwrap();
    // Repeating it (another reconcile) changes nothing.
    s.test
        .db
        .record_live_checkpoint_revision(Some(&base), &revision_record(&s, &written), &checkpoint)
        .await
        .unwrap();

    let rows = s.test.db.revisions(s.doc).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.revision.as_str())
            .collect::<Vec<_>>(),
        [base.as_str(), written.as_str()]
    );
    assert_eq!(rows[1].parent.as_deref(), Some(base.as_str()));
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.unverified_checkpoints.len(), 1);
    assert_eq!(loaded.unverified_checkpoints[0].revision, written);
    // The projection appending the same revision afterwards is not a conflict.
    assert!(
        s.test
            .db
            .append_revision(Some(&base), &revision_record(&s, &written))
            .await
            .unwrap()
    );
    assert_eq!(s.test.db.revisions(s.doc).await.unwrap().len(), 2);
}

#[tokio::test]
async fn checkpoint_revision_after_another_head_stores_nothing() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let base = revision(&s, 'a', None).await;
    let _outside = revision(&s, 'c', Some(&base)).await;
    let written = "b".repeat(40);
    let checkpoint = written_checkpoint(&written, session);

    assert!(
        s.test
            .db
            .record_live_checkpoint_revision(
                Some(&base),
                &revision_record(&s, &written),
                &checkpoint
            )
            .await
            .is_err()
    );
    assert_eq!(s.test.db.revisions(s.doc).await.unwrap().len(), 2);
    assert!(
        s.test
            .db
            .load_live(s.doc)
            .await
            .unwrap()
            .unverified_checkpoints
            .is_empty()
    );
}

#[tokio::test]
async fn migration_0020_backfills_tombstones_in_one_pass() {
    // 0019 is the migration before 0020 on this branch.
    let test = TestDb::create_at(Some(19)).await;
    let ann = user(&test, "Ann", false).await.id;
    let doc = tree(&test, ann).await.doc;
    let mut expected = Vec::new();
    let mut seq = 0;
    for n in 0..24_u64 {
        let session = Uuid::new_v4();
        test.db
            .start_live_session(doc, session, FORMAT, at(0))
            .await
            .unwrap();
        let mut places = Vec::new();
        for _ in 0..2 {
            let one = client();
            let number = test
                .db
                .add_live_participant(doc, session, one, ann, at(0))
                .await
                .unwrap();
            places.push((number, one));
        }
        // Every third session stores nothing, so its final position is the head it began at.
        let stored = if n % 3 == 0 { 0 } else { 3 };
        for local in 1..=stored {
            seq += 1;
            test.db
                .append_live_change(
                    doc,
                    &change(seq, session, places[0].1, ann, local, b"x", &[]),
                )
                .await
                .unwrap();
        }
        let mut conn = sqlx::PgConnection::connect(&test.url()).await.unwrap();
        sqlx::query("update live_sessions set ended_at = $2 where id = $1")
            .bind(session)
            .bind(at(1))
            .execute(&mut conn)
            .await
            .unwrap();
        for (number, one) in places {
            expected.push((session, number, one, seq));
        }
    }

    // Run 0020 inside one transaction first, to count how often it reads the entries.
    let mut conn = sqlx::PgConnection::connect(&test.url()).await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0020_live_checkpoints.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();
    let reads: i64 = sqlx::query_scalar(
        "select (coalesce(seq_scan, 0) + coalesce(idx_scan, 0))::bigint \
         from pg_stat_xact_user_tables \
         where relname = 'live_entries' and schemaname = current_schema()",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        reads <= 2,
        "the backfill read the entries {reads} times for {} ended places",
        expected.len()
    );

    test.db.migrate().await.unwrap();
    for (session, participant, one, final_seq) in expected {
        let tombstone = test
            .db
            .live_return_tombstone(doc, session, participant)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tombstone.client, one);
        assert_eq!(tombstone.final_seq, Seq(final_seq), "session {session}");
    }
}

/// Records `count` changes and an unverified checkpoint after each, with `data` as session
/// data; answers the revisions, oldest first.
async fn checkpoint_each_change(s: &Setup, session: Uuid, count: u64, data: &[u8]) -> Vec<String> {
    let one = client();
    let mut revisions = Vec::new();
    let mut parent: Option<String> = None;
    for seq in 1..=count {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
        let id = char::from(b'a' + u8::try_from(seq).unwrap());
        let revision = revision(s, id, parent.as_deref()).await;
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision: &revision,
                    through: Seq(seq),
                    sha256: [u8::try_from(seq).unwrap(); 32],
                    session,
                    session_data: data,
                    major_reason: None,
                    major_person: None,
                    writer: None,
                    created_at: at(i64::try_from(seq).unwrap() + 100),
                },
            )
            .await
            .unwrap();
        parent = Some(revision.clone());
        revisions.push(revision);
    }
    revisions
}

#[tokio::test]
async fn unverified_checkpoints_are_capped_per_document() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let count = u64::try_from(records::MAX_UNVERIFIED_CHECKPOINTS).unwrap() + 3;
    let revisions = checkpoint_each_change(&s, session, count, b"identity table").await;

    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    let kept = loaded
        .unverified_checkpoints
        .iter()
        .map(|checkpoint| checkpoint.revision.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        kept,
        revisions[revisions.len() - records::MAX_UNVERIFIED_CHECKPOINTS..],
        "only the newest unverified checkpoints are kept"
    );
    let rows: i64 =
        sqlx::query_scalar("select count(*) from live_checkpoints where document_id = $1")
            .bind(s.doc.0)
            .fetch_one(s.test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(
        rows,
        i64::try_from(records::MAX_UNVERIFIED_CHECKPOINTS).unwrap()
    );
}

#[tokio::test]
async fn verification_drops_superseded_session_data() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let revisions = checkpoint_each_change(&s, session, 3, b"identity table").await;

    s.test
        .db
        .verify_live_checkpoint(s.doc, &revisions[1], Seq(2), at(50))
        .await
        .unwrap();

    let data: Vec<(String, i32)> = sqlx::query_as(
        "select revision, octet_length(session_data) from live_checkpoints \
         where document_id = $1 order by through",
    )
    .bind(s.doc.0)
    .fetch_all(s.test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(
        data,
        [(revisions[1].clone(), 14), (revisions[2].clone(), 14),],
        "the superseded unverified checkpoint is gone; the verified base and the newer one keep \
         their identity data"
    );
}

#[tokio::test]
async fn checkpoint_session_data_counts_against_the_site_quota() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await.id;
    let tree = tree(&test, ann).await;
    let s = Setup {
        bob: ann,
        ann,
        doc: tree.doc,
        test,
    };
    let session = session(&s, 0).await;
    checkpoint_each_change(&s, session, 2, &[7; 1_000]).await;
    let with_data = s.test.db.site_usage(tree.site).await.unwrap();
    sqlx::query("update live_checkpoints set session_data = '' where document_id = $1")
        .bind(s.doc.0)
        .execute(s.test.db.test_pool())
        .await
        .unwrap();
    let without = s.test.db.site_usage(tree.site).await.unwrap();
    assert_eq!(
        with_data.used - without.used,
        2_000,
        "identity data is stored bytes"
    );
}

/// An outside revision stored again on top of a live log becomes its verified catch-up base:
/// the live checkpoints it replaces go, and the entries it covers count as saved.
#[tokio::test]
async fn adopted_outside_base_becomes_the_verified_catchup_base() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=3 {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
    }
    let live = revision(&s, 'a', None).await;
    s.test
        .db
        .record_live_checkpoint(
            s.doc,
            &NewLiveCheckpoint {
                revision: &live,
                through: Seq(2),
                sha256: [1; 32],
                session,
                session_data: b"ids",
                major_reason: None,
                major_person: None,
                writer: None,
                created_at: at(4),
            },
        )
        .await
        .unwrap();
    let outside = revision(&s, 'b', Some(&live)).await;

    s.test
        .db
        .adopt_live_base(
            s.doc,
            &NewLiveCheckpoint {
                revision: &outside,
                through: Seq(3),
                sha256: [2; 32],
                session,
                session_data: b"",
                major_reason: Some("outside_replacement"),
                major_person: Some(s.bob),
                writer: None,
                created_at: at(5),
            },
            at(6),
        )
        .await
        .unwrap();
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    assert_eq!(loaded.verified_through, Seq(3));
    let base = loaded.verified_checkpoint.unwrap();
    assert_eq!(
        (base.revision.as_str(), base.through),
        (outside.as_str(), Seq(3))
    );
    assert!(
        base.session_data.is_empty(),
        "a plain file base has no identities"
    );
    assert!(loaded.unverified_checkpoints.is_empty());
    assert!(loaded.entries.is_empty());
    assert_eq!(loaded.unsaved_entries, 0);
    assert_eq!(loaded.head, Seq(3), "the log keeps its positions");

    // Beyond the head is refused.
    assert!(
        s.test
            .db
            .adopt_live_base(
                s.doc,
                &NewLiveCheckpoint {
                    revision: &outside,
                    through: Seq(4),
                    sha256: [2; 32],
                    session,
                    session_data: b"",
                    major_reason: None,
                    major_person: None,
                    writer: None,
                    created_at: at(7),
                },
                at(7),
            )
            .await
            .is_err()
    );
}

/// Major-version checkpoints are history people restore from: the cap on unverified
/// checkpoints never drops one.
#[tokio::test]
async fn unverified_cap_keeps_major_checkpoints() {
    let s = setup().await;
    let session = session(&s, 0).await;
    let one = client();
    for seq in 1..=6 {
        s.test
            .db
            .append_live_change(s.doc, &change(seq, session, one, s.ann, seq, b"x", &[]))
            .await
            .unwrap();
    }
    let mut parent: Option<String> = None;
    for (n, letter) in ['a', 'b', 'c', 'd', 'e', 'f'].into_iter().enumerate() {
        let written = revision(&s, letter, parent.as_deref()).await;
        let through = u64::try_from(n).unwrap() + 1;
        s.test
            .db
            .record_live_checkpoint(
                s.doc,
                &NewLiveCheckpoint {
                    revision: &written,
                    through: Seq(through),
                    sha256: [1; 32],
                    session,
                    session_data: b"ids",
                    major_reason: (n == 0).then_some("returning_client_before"),
                    major_person: (n == 0).then_some(s.bob),
                    writer: None,
                    created_at: at(i64::try_from(through).unwrap() + 10),
                },
            )
            .await
            .unwrap();
        parent = Some(written);
    }
    let loaded = s.test.db.load_live(s.doc).await.unwrap();
    let kept = loaded
        .unverified_checkpoints
        .iter()
        .map(|checkpoint| checkpoint.through.0)
        .collect::<Vec<_>>();
    assert_eq!(
        kept,
        [1, 3, 4, 5, 6],
        "the major checkpoint stays; ordinary ones are capped"
    );
}
