//! Live rooms over real WebSockets with real `collab-live` clients and the stand-in editor.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::{DocId, Seq};
use collab_live::client::{Event, LocalError, Mode, Phase};
use collab_live::stand_in::StandInOp;
use common::live::{FORMAT, LiveTestClient};
use common::{
    FileClient, Site, StoppableServer, TestClient, TestServer, TestUser, new_doc,
    spawn_server_config, wait_events,
};
use protocol::RemovedReason;
use protocol::live::{EntryBody, LiveClientMsg, LiveServerMsg, LocalNo, Place, RefusedReason};
use records::{Editor, LiveEntryRow};
use reqwest::StatusCode;
use tempfile::tempdir;

fn live_config(config: &mut server::Config) {
    config.live_editors = vec![Editor::GridCraft];
    config.live_heartbeat = Duration::from_secs(1);
}

async fn live_server() -> TestServer {
    spawn_server_config(live_config).await
}

async fn shared_live(server: &TestServer) -> (DocId, TestUser, TestUser) {
    let doc = new_doc();
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    server.create_file_document(doc, &ann).await;
    share(server, &ann, doc, &bob, "editor").await;
    (doc, ann, bob)
}

async fn share(site: &Site, owner: &TestUser, doc: DocId, user: &TestUser, role: &str) {
    let response = site
        .http(Some(owner))
        .put(site.url(&format!("/api/docs/{}/access", doc.0)))
        .json(&serde_json::json!({ "email": user.email, "role": role }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

async fn revoke(site: &Site, owner: &TestUser, doc: DocId, user: &TestUser) {
    let response = site
        .http(Some(owner))
        .delete(site.url(&format!("/api/docs/{}/access/user/{}", doc.0, user.id.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

fn assert_stored_client_entry(sent: &LiveClientMsg, stored: &LiveEntryRow) {
    match (sent, stored) {
        (
            LiveClientMsg::Entry {
                local,
                seen,
                format,
                payload,
            },
            LiveEntryRow::Change {
                local: stored_local,
                seen: stored_seen,
                format: stored_format,
                payload: stored_payload,
                ..
            },
        ) => {
            assert_eq!(local.0, *stored_local);
            assert_eq!(*seen, *stored_seen);
            assert_eq!(format, stored_format);
            assert_eq!(payload.as_bytes(), stored_payload);
        }
        (sent, stored) => panic!("client entry {sent:?} does not match stored row {stored:?}"),
    }
}

#[tokio::test]
async fn two_clients_converge_on_stand_in() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    for n in 0..20 {
        a.insert(&format!("a{n}")).await;
        b.insert(&format!("b{n}")).await;
    }
    a.pump_until(|c| c.client().agreed_seq() == Seq(40) && c.client().pending() == 0)
        .await;
    b.pump_until(|c| c.client().agreed_seq() == Seq(40) && c.client().pending() == 0)
        .await;
    assert_eq!(a.lines(), b.lines());
    assert_eq!(a.lines().len(), 40);
}

#[tokio::test]
async fn racing_entries_get_one_order() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    let anchor = a.insert("anchor").await;
    a.pump_until(|c| c.client().pending() == 0).await;
    b.pump_until(|c| c.client().agreed_seq() == Seq(1)).await;
    a.insert_before(Some(anchor), "ann").await;
    b.insert_before(Some(anchor), "bob").await;
    a.pump_until(|c| c.client().agreed_seq() == Seq(3) && c.client().pending() == 0)
        .await;
    b.pump_until(|c| c.client().agreed_seq() == Seq(3) && c.client().pending() == 0)
        .await;
    assert_eq!(a.lines(), b.lines());
    let loaded = server.records_db().load_live(doc).await.unwrap();
    let later = match &loaded.entries[2] {
        LiveEntryRow::Change { payload, .. } => String::from_utf8(payload.clone()).unwrap(),
        other => panic!("later entry is {other:?}"),
    };
    let later_text = if later.contains("ann") { "ann" } else { "bob" };
    let at = a
        .lines()
        .iter()
        .position(|line| line == later_text)
        .unwrap();
    assert_eq!(a.lines().get(at + 1).map(String::as_str), Some("anchor"));
}

#[tokio::test]
async fn reconnect_resends_without_duplicating() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    for text in ["one", "two", "three"] {
        a.insert(text).await;
    }
    b.pump_until(|c| c.client().agreed_seq() == Seq(3)).await;
    a.disconnect();
    a.reconnect(&server).await;
    a.pump_until(|c| c.client().phase() == Phase::Live && c.client().pending() == 0)
        .await;
    let loaded = server.records_db().load_live(doc).await.unwrap();
    let locals = loaded
        .entries
        .iter()
        .filter_map(|row| match row {
            LiveEntryRow::Change { client, local, .. } if *client == a.client().client() => {
                Some(*local)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(locals, vec![1, 2, 3]);
}

#[tokio::test]
async fn resent_entry_reaching_the_server_twice_is_stored_once() {
    let server = live_server().await;
    let (doc, ann, _) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    a.drop_next_server_entry_frame();
    a.insert("once").await;
    let original = a.outbound_messages.last().unwrap().clone();
    a.pump_without_tick_until(|client| client.dropped_server_entry_frames() == 1)
        .await;
    assert_eq!(a.client().pending(), 1);
    let before_resend = server.records_db().load_live(doc).await.unwrap();
    assert_eq!(before_resend.head, Seq(1));

    a.disconnect();
    a.reconnect(&server).await;
    assert_eq!(a.client().pending(), 1);
    a.resend_entry(&original).await;
    assert_eq!(
        a.outbound_messages
            .iter()
            .filter(|message| **message == original)
            .count(),
        2
    );
    a.send_raw(&serde_json::to_string(&LiveClientMsg::Leave).unwrap())
        .await;
    a.wait_closed().await;

    let after_resend = server.records_db().load_live(doc).await.unwrap();
    let stored = after_resend
        .entries
        .iter()
        .filter(|row| matches!(row, LiveEntryRow::Change { client, local: 1, .. } if *client == a.client().client()))
        .collect::<Vec<_>>();
    assert_eq!(stored.len(), 1);
    assert_stored_client_entry(&original, stored[0]);
    assert_eq!(after_resend.head, before_resend.head);
    assert_eq!(a.client().pending(), 0);
    assert!(!a.messages.iter().any(|message| matches!(
        message,
        LiveServerMsg::Refused {
            local: LocalNo(1),
            reason: RefusedReason::OutOfOrder,
        }
    )));
}

#[tokio::test]
async fn expiry_with_entry_in_flight_commits_once() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    b.pump_until(|client| {
        client.messages.iter().any(|message| {
            matches!(message, LiveServerMsg::People { people } if people.iter().all(|person| person.user != ann.id))
        })
    })
    .await;
    a.insert("in flight").await;
    let original = a.outbound_messages.last().unwrap().clone();
    a.pump_without_tick_until(|client| client.has_event(|event| matches!(event, Event::Expired)))
        .await;
    assert_eq!(a.client().pending(), 1);
    let sent_before_reconnect = a.outbound_messages.len();
    a.reconnect(&server).await;
    a.pump_until(|c| c.client().phase() == Phase::Live).await;
    assert_eq!(a.client().pending(), 1);
    a.receive_server_message(&LiveServerMsg::ReturnReady {
        through: LocalNo(1),
    })
    .await;
    a.pump_until(|c| c.client().phase() == Phase::Live && c.client().pending() == 0)
        .await;
    assert!(matches!(a.last_place(), Some(Place::Returning { .. })));
    assert!(a.outbound_messages[sent_before_reconnect..].contains(&original));
    let loaded = server.records_db().load_live(doc).await.unwrap();
    let stored = loaded
        .entries
        .iter()
        .filter(|row| matches!(row, LiveEntryRow::Change { client, local: 1, .. } if *client == a.client().client()))
        .collect::<Vec<_>>();
    assert_eq!(stored.len(), 1);
    assert_stored_client_entry(&original, stored[0]);
    assert_eq!(a.client().pending(), 0);
}

#[tokio::test]
async fn log_survives_server_restart() {
    let data = tempdir().unwrap();
    let db = records::test_support::TestDb::create().await;
    let mut first = StoppableServer::start(data.path(), &db, live_config);
    let doc = new_doc();
    let ann = first.user("Ann").await;
    first.create_file_document(doc, &ann).await;
    let bob = first.user("Bob").await;
    share(&first, &ann, doc, &bob, "editor").await;
    let mut a = LiveTestClient::connect(&first, doc, &ann, "failing-device").await;
    let mut b = LiveTestClient::connect(&first, doc, &bob, "failing-device").await;
    for text in ["one", "two"] {
        a.insert(text).await;
    }
    a.pump_until(|c| c.client().agreed_seq() == Seq(2) && c.client().pending() == 0)
        .await;
    b.pump_until(|c| c.client().agreed_seq() == Seq(2)).await;
    a.change(
        StandInOp::CrashIf {
            device: "failing-device".into(),
        },
        Mode::WaitForServer,
    )
    .await
    .unwrap();
    a.pump_until(|c| matches!(c.client().phase(), Phase::Stalled { at: Seq(3) }))
        .await;
    b.pump_until(|c| matches!(c.client().phase(), Phase::Stalled { at: Seq(3) }))
        .await;
    a.pump_until(|c| c.client().agreed_seq() == Seq(4) && c.client().pending() == 0)
        .await;
    b.pump_until(|c| c.client().agreed_seq() == Seq(4)).await;
    a.drop_next_entry_frame();
    a.insert("three").await;
    let unconfirmed = a.outbound_messages.last().unwrap().clone();
    assert_eq!(a.client().pending(), 1);
    let before = first.records_db().load_live(doc).await.unwrap();
    first.stop();

    let second = StoppableServer::start(data.path(), &db, live_config);
    let cy = second.user("Cy").await;
    share(&second, &ann, doc, &cy, "editor").await;
    let c = LiveTestClient::connect(&second, doc, &cy, "c").await;
    assert_eq!(c.lines(), vec!["one", "two"]);
    let caught_up = c
        .messages
        .iter()
        .filter_map(|message| match message {
            LiveServerMsg::Entry { entry } => Some(entry),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(caught_up.len(), before.entries.len());
    for (entry, row) in caught_up.iter().zip(&before.entries) {
        match (&entry.body, row) {
            (
                EntryBody::Change {
                    participant,
                    client,
                    local,
                    seen,
                    payload,
                    cancelled,
                },
                LiveEntryRow::Change {
                    seq,
                    client: stored_client,
                    local: stored_local,
                    participant: stored_participant,
                    seen: stored_seen,
                    at,
                    seed,
                    payload: stored_payload,
                    cancelled_by,
                    ..
                },
            ) => {
                assert_eq!(entry.seq, *seq);
                assert_eq!(*client, *stored_client);
                assert_eq!(local.0, *stored_local);
                assert_eq!(participant.0, *stored_participant);
                assert_eq!(
                    i128::from(entry.at_ms),
                    at.unix_timestamp_nanos() / 1_000_000
                );
                assert_eq!(entry.seed, *seed);
                assert_eq!(*seen, *stored_seen);
                assert_eq!(*cancelled, cancelled_by.is_some());
                if cancelled_by.is_some() {
                    assert!(payload.is_empty());
                } else {
                    assert_eq!(payload.as_bytes(), stored_payload);
                }
            }
            (
                EntryBody::Cancel { cancels, reports },
                LiveEntryRow::Cancel {
                    seq,
                    at,
                    seed,
                    cancels: stored_cancels,
                    reports: stored_reports,
                    ..
                },
            ) => {
                assert_eq!(entry.seq, *seq);
                assert_eq!(
                    i128::from(entry.at_ms),
                    at.unix_timestamp_nanos() / 1_000_000
                );
                assert_eq!(entry.seed, *seed);
                assert_eq!(*cancels, *stored_cancels);
                assert_eq!(*reports, *stored_reports);
            }
            (body, row) => panic!("caught-up entry {body:?} does not match stored row {row:?}"),
        }
    }
    let after = second.records_db().load_live(doc).await.unwrap();
    assert_eq!(before.entries, after.entries);

    let sent_before_reconnect = a.outbound_messages.len();
    a.reconnect(&second).await;
    a.pump_until(|c| c.client().phase() == Phase::Live && c.client().pending() == 0)
        .await;
    assert_eq!(a.last_place(), Some(Place::Kept));
    assert!(a.outbound_messages[sent_before_reconnect..].contains(&unconfirmed));
    let after_resend = second.records_db().load_live(doc).await.unwrap();
    assert_eq!(after_resend.head, Seq(5));
    assert_eq!(
        &after_resend.entries[..before.entries.len()],
        before.entries.as_slice()
    );
    assert_eq!(after_resend.entries.len(), before.entries.len() + 1);
    let resent = after_resend
        .entries
        .iter()
        .filter(|row| matches!(row, LiveEntryRow::Change { client, local: 4, .. } if *client == a.client().client()))
        .collect::<Vec<_>>();
    assert_eq!(resent.len(), 1);
    assert_stored_client_entry(&unconfirmed, resent[0]);
    assert_eq!(a.client().pending(), 0);
    a.insert("four").await;
    a.pump_until(|c| c.client().agreed_seq() == Seq(6) && c.client().pending() == 0)
        .await;
    assert_eq!(
        second.records_db().load_live(doc).await.unwrap().head,
        Seq(6)
    );
}

#[tokio::test]
async fn malformed_payload_is_relayed_verbatim_and_does_nothing() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    for payload in ["not json", "{\"type\":\"live_joined\"}"] {
        a.payload(payload.to_owned(), Mode::ShowAtOnce)
            .await
            .unwrap();
    }
    a.pump_until(|c| c.client().agreed_seq() == Seq(2) && c.client().pending() == 0)
        .await;
    b.pump_until(|c| c.client().agreed_seq() == Seq(2)).await;
    assert!(a.lines().is_empty() && b.lines().is_empty());
    let relayed = b
        .messages
        .iter()
        .filter_map(|message| match message {
            LiveServerMsg::Entry {
                entry:
                    protocol::live::LiveEntry {
                        body: EntryBody::Change { payload, .. },
                        ..
                    },
            } => Some(payload.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(relayed, vec!["not json", "{\"type\":\"live_joined\"}"]);
}

#[tokio::test]
async fn garbage_frame_closes_only_that_connection() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    a.send_raw("}{").await;
    a.wait_closed().await;
    b.insert("still live").await;
    b.pump_until(|c| c.client().pending() == 0).await;
    assert_eq!(b.lines(), vec!["still live"]);
}

#[tokio::test]
async fn malformed_fingerprints_are_refused_without_ending_the_room() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;

    for entered in [
        "a".repeat(63),
        "a".repeat(65),
        format!("{}A", "a".repeat(63)),
        format!("{}g", "a".repeat(63)),
    ] {
        let mut sender = LiveTestClient::connect(&server, doc, &ann, "sender").await;
        sender
            .send_raw(
                &serde_json::to_string(&LiveClientMsg::Fingerprint {
                    at: Seq(0),
                    entered,
                })
                .unwrap(),
            )
            .await;
        sender.wait_closed().await;
    }

    let mut valid = LiveTestClient::connect(&server, doc, &ann, "valid").await;
    valid
        .send_raw(
            &serde_json::to_string(&LiveClientMsg::Fingerprint {
                at: Seq(0),
                entered: "0123456789abcdef".repeat(4),
            })
            .unwrap(),
        )
        .await;
    b.insert("still live").await;
    b.pump_until(|client| client.client().pending() == 0).await;
    valid
        .pump_until(|client| client.lines() == ["still live"])
        .await;
    assert_eq!(b.lines(), ["still live"]);
    assert_eq!(
        server.records_db().load_live(doc).await.unwrap().head,
        Seq(1)
    );
}

#[tokio::test]
async fn out_of_range_local_is_refused_without_ending_the_room() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    let hostile = LiveClientMsg::Entry {
        local: LocalNo(i64::MAX as u64 + 1),
        seen: Seq(0),
        format: FORMAT.to_owned(),
        payload: "hostile".to_owned(),
    };

    a.send_raw(&serde_json::to_string(&hostile).unwrap()).await;
    while !a.messages.iter().any(|message| {
        matches!(
            message,
            LiveServerMsg::Refused {
                local: LocalNo(local),
                reason: RefusedReason::OutOfOrder,
            } if *local == i64::MAX as u64 + 1
        )
    }) {
        a.receive_raw_only().await;
    }

    b.insert("still live").await;
    b.pump_until(|client| client.client().pending() == 0).await;
    a.pump_until(|client| client.lines() == ["still live"])
        .await;
    assert_eq!(
        server.records_db().load_live(doc).await.unwrap().head,
        Seq(1)
    );
}

#[tokio::test]
async fn viewer_receives_but_cannot_send() {
    let server = live_server().await;
    let doc = new_doc();
    let ann = server.user("Ann").await;
    let vic = server.user("Vic").await;
    server.create_file_document(doc, &ann).await;
    share(&server, &ann, doc, &vic, "viewer").await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut v = LiveTestClient::connect(&server, doc, &vic, "v").await;
    assert_eq!(
        v.payload("no".to_owned(), Mode::ShowAtOnce).await,
        Err(LocalError::ReadOnly)
    );
    let raw = LiveClientMsg::Entry {
        local: LocalNo(1),
        seen: Seq(0),
        format: FORMAT.to_owned(),
        payload: "hostile".to_owned(),
    };
    v.send_raw(&serde_json::to_string(&raw).unwrap()).await;
    while !v.messages.iter().any(|message| {
        matches!(
            message,
            LiveServerMsg::Refused {
                reason: RefusedReason::ReadOnly,
                ..
            }
        )
    }) {
        v.receive_raw_only().await;
    }
    a.insert("visible").await;
    v.pump_until(|c| c.lines() == ["visible"] && c.client().agreed_seq() == Seq(1))
        .await;
}

#[tokio::test]
async fn revoking_access_removes_from_live_room() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    revoke(&server, &ann, doc, &bob).await;
    b.pump_until(|c| {
        c.has_event(|event| {
            matches!(
                event,
                Event::Removed {
                    reason: RemovedReason::AccessRevoked
                }
            )
        })
    })
    .await;
    b.wait_closed().await;
    a.pump_until(|c| {
        c.messages.iter().any(|message| matches!(message, LiveServerMsg::People { people } if people.iter().all(|person| person.user != bob.id)))
    })
    .await;
}

#[tokio::test]
async fn revoking_a_disconnected_place_ends_it_as_removed() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let _a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    let participant = b.client().participant().unwrap().0;
    b.disconnect();
    revoke(&server, &ann, doc, &bob).await;
    let removed = tokio::time::timeout(common::TIMEOUT, async {
        loop {
            let loaded = server.records_db().load_live(doc).await.unwrap();
            if loaded.participants.iter().any(|place| {
                place.number == participant && place.gone == Some(records::GoneReason::Removed)
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if removed.is_err() {
        let loaded = server.records_db().load_live(doc).await.unwrap();
        panic!("disconnected place was not removed promptly: {loaded:?}");
    }
}

#[tokio::test]
async fn lowered_role_drops_waiting_changes() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    for n in 0..21 {
        b.insert(&format!("b{n}")).await;
    }
    share(&server, &ann, doc, &bob, "viewer").await;
    b.pump_until(|c| {
        c.has_event(|event| matches!(event, Event::DroppedPending { count } if *count > 0))
    })
    .await;
    assert_eq!(b.client().pending(), 0);
    assert!(!b.client().can_edit());
}

#[tokio::test]
async fn crash_on_some_devices_is_cancelled() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "failing-device").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "failing-device").await;
    a.change(
        StandInOp::CrashIf {
            device: "failing-device".into(),
        },
        Mode::WaitForServer,
    )
    .await
    .unwrap();
    a.change(
        StandInOp::CrashIf {
            device: "failing-device".into(),
        },
        Mode::WaitForServer,
    )
    .await
    .unwrap();
    b.pump_until(|c| matches!(c.client().phase(), Phase::Stalled { at: Seq(1) }))
        .await;
    a.pump_until(|c| matches!(c.client().phase(), Phase::Stalled { at: Seq(1) }))
        .await;
    a.pump_until(|c| matches!(c.client().phase(), Phase::Stalled { at: Seq(2) }))
        .await;
    b.pump_until(|c| matches!(c.client().phase(), Phase::Stalled { at: Seq(2) }))
        .await;
    a.pump_until(|c| c.client().agreed_seq().0 >= 4).await;
    b.pump_until(|c| c.client().agreed_seq().0 >= 4).await;
    let cy = server.user("Cy").await;
    share(&server, &ann, doc, &cy, "editor").await;
    let c = LiveTestClient::connect(&server, doc, &cy, "c").await;
    let cancels = c
        .messages
        .iter()
        .filter(|message| matches!(message, LiveServerMsg::Entry { entry } if matches!(entry.body, EntryBody::Cancel { .. })))
        .count();
    assert_eq!(cancels, 2);
}

#[tokio::test]
async fn crash_on_one_device_stands() {
    let server = live_server().await;
    let (doc, ann, bob) = shared_live(&server).await;
    let cy = server.user("Cy").await;
    share(&server, &ann, doc, &cy, "editor").await;
    let mut a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    let mut b = LiveTestClient::connect(&server, doc, &bob, "b").await;
    let mut c = LiveTestClient::connect(&server, doc, &cy, "c").await;
    a.change(
        StandInOp::CrashIf { device: "b".into() },
        Mode::WaitForServer,
    )
    .await
    .unwrap();
    a.pump_until(|client| client.client().agreed_seq() == Seq(1))
        .await;
    c.pump_until(|client| client.client().agreed_seq() == Seq(1))
        .await;
    b.pump_until(|client| matches!(client.client().phase(), Phase::Stalled { at: Seq(1) }))
        .await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    a.tick_now().await;
    b.pump_until(|client| client.client().phase() == Phase::Closed)
        .await;
    assert!(b.has_event(|event| matches!(event, Event::CannotOpen { seq: Seq(1) })));
}

#[tokio::test]
async fn text_and_file_documents_are_unchanged() {
    let server = spawn_server_config(|config| {
        assert!(config.live_editors.is_empty());
    })
    .await;
    let ann = server.user("Ann").await;
    let file = new_doc();
    server.create_file_document(file, &ann).await;
    let file_client = FileClient::connect_as(&server, file, &ann).await;
    assert!(file_client.joined);
    let text = new_doc();
    server.give_access(text, &ann).await;
    let text_client = TestClient::connect_as(&server, text, &ann).await;
    assert!(text_client.joined);
}

#[tokio::test]
async fn events_are_recorded() {
    let server = live_server().await;
    let doc = new_doc();
    let ann = server.user("Ann").await;
    server.create_file_document(doc, &ann).await;
    let _a = LiveTestClient::connect(&server, doc, &ann, "a").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let events = wait_events(&server.db.db, doc, |events| {
        [
            "live.session_started",
            "live.client_expired",
            "live.session_ended",
        ]
        .iter()
        .all(|kind| events.iter().any(|event| event.kind == *kind))
    })
    .await;
    for event in events
        .iter()
        .filter(|event| event.kind.starts_with("live."))
    {
        let detail = event.detail.to_string();
        assert!(!detail.contains("payload"));
        assert!(!detail.contains("secret text"));
        match event.kind.as_str() {
            "live.session_started" => {
                assert_eq!(event.detail, serde_json::json!({ "format": FORMAT }))
            }
            "live.client_expired" => {
                assert_eq!(event.detail, serde_json::json!({ "participant": 1 }))
            }
            "live.session_ended" => {
                assert_eq!(event.detail, serde_json::json!({ "head": 0, "entries": 0 }))
            }
            other => panic!("unexpected live event {other}"),
        }
    }
}

/// Reads live frames from `socket` until `found` picks one.
async fn next_live<T>(
    socket: &mut common::Socket,
    found: impl Fn(LiveServerMsg) -> Option<T>,
) -> T {
    use futures_util::StreamExt;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    if let Some(value) = found(serde_json::from_str(&text).unwrap()) {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                other => panic!("socket ended: {other:?}"),
            }
        }
    })
    .await
    .expect("live frame timed out")
}

async fn send_live(socket: &mut common::Socket, message: &LiveClientMsg) {
    use futures_util::SinkExt;
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(
            serde_json::to_string(message).unwrap(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn participant_checkpoint_upload_becomes_the_current_revision() {
    let server = spawn_server_config(|config| {
        live_config(config);
        config.live_checkpoint_quiet = Duration::from_millis(50);
    })
    .await;
    let (doc, ann, _bob) = shared_live(&server).await;
    let mut socket = common::open_socket(server.addr, doc, &ann).await;
    send_live(
        &mut socket,
        &LiveClientMsg::LiveJoin {
            client: collab_core::ids::ClientId(uuid::Uuid::new_v4()),
            protocol: protocol::live::LIVE_PROTOCOL,
            format: FORMAT.to_owned(),
            resume: None,
            seen: None,
            can_checkpoint: true,
            build: protocol::live::LiveBuild::Browser,
            returning_through: None,
        },
    )
    .await;
    next_live(&mut socket, |message| {
        matches!(message, LiveServerMsg::LiveJoined { .. }).then_some(())
    })
    .await;
    send_live(
        &mut socket,
        &LiveClientMsg::Entry {
            local: LocalNo(1),
            seen: Seq(0),
            format: FORMAT.to_owned(),
            payload: "one".to_owned(),
        },
    )
    .await;
    let (ticket, at, base) = next_live(&mut socket, |message| match message {
        LiveServerMsg::CheckpointTicket {
            ticket, at, base, ..
        } => Some((ticket, at, base)),
        _ => None,
    })
    .await;
    assert_eq!(at, Seq(1));
    let session_data = b"stand-in identities".to_vec();
    let file = b"checkpoint workbook bytes".to_vec();
    let mut body = session_data.clone();
    body.extend_from_slice(&file);
    let mut query = vec![("ticket", ticket.to_string()), ("at", at.0.to_string())];
    if let Some(base) = &base {
        query.push(("base", base.clone()));
    }

    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/docs/{}/live-checkpoint", doc.0)))
        .query(&query)
        .header("x-sharecraft-session-data-length", session_data.len())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let revision = response.json::<serde_json::Value>().await.unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let downloaded = server
        .http(Some(&ann))
        .get(server.url(&format!("/api/docs/{}/content", doc.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(downloaded.status(), StatusCode::OK);
    assert_eq!(downloaded.bytes().await.unwrap().as_ref(), file.as_slice());
    let loaded = server.db.db.load_live(doc).await.unwrap();
    assert_eq!(loaded.verified_through, Seq(0));
    assert_eq!(loaded.unverified_checkpoints.len(), 1);
    assert_eq!(loaded.unverified_checkpoints[0].revision, revision);
    assert_eq!(loaded.unverified_checkpoints[0].session_data, session_data);

    let reused = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/docs/{}/live-checkpoint", doc.0)))
        .query(&query)
        .header("x-sharecraft-session-data-length", session_data.len())
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        reused.status(),
        StatusCode::FORBIDDEN,
        "a ticket is one-use"
    );
}
