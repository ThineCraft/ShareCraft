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
    // The server stores the current file as a major version, then lets the batch go.
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

async fn join_capable_socket(
    server: &Site,
    doc: DocId,
    user: &TestUser,
    build: protocol::live::LiveBuild,
) -> common::Socket {
    let mut socket = common::open_socket(server.addr, doc, user).await;
    send_live(
        &mut socket,
        &LiveClientMsg::LiveJoin {
            client: collab_core::ids::ClientId(uuid::Uuid::new_v4()),
            protocol: protocol::live::LIVE_PROTOCOL,
            format: FORMAT.to_owned(),
            resume: None,
            seen: None,
            can_checkpoint: true,
            build,
            returning_through: None,
        },
    )
    .await;
    next_live(&mut socket, |message| {
        matches!(message, LiveServerMsg::LiveJoined { .. }).then_some(())
    })
    .await;
    socket
}

#[tokio::test]
async fn second_participant_hash_verifies_the_checkpoint() {
    use sha2::Digest;
    let server = spawn_server_config(|config| {
        live_config(config);
        config.live_checkpoint_quiet = Duration::from_millis(50);
    })
    .await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut ann_socket =
        join_capable_socket(&server, doc, &ann, protocol::live::LiveBuild::Browser).await;
    let mut bob_socket =
        join_capable_socket(&server, doc, &bob, protocol::live::LiveBuild::Desktop).await;
    send_live(
        &mut ann_socket,
        &LiveClientMsg::Entry {
            local: LocalNo(1),
            seen: Seq(0),
            format: FORMAT.to_owned(),
            payload: "one".to_owned(),
        },
    )
    .await;
    let (ticket, at) = next_live(&mut ann_socket, |message| match message {
        LiveServerMsg::CheckpointTicket { ticket, at, .. } => Some((ticket, at)),
        _ => None,
    })
    .await;
    let file = b"checkpoint workbook bytes".to_vec();
    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/docs/{}/live-checkpoint", doc.0)))
        .query(&[("ticket", ticket.to_string()), ("at", at.0.to_string())])
        .header("x-sharecraft-session-data-length", 3)
        .body([b"ids".as_slice(), &file].concat())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let revision = response.json::<serde_json::Value>().await.unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let (verify, purpose, verify_at) = next_live(&mut bob_socket, |message| match message {
        LiveServerMsg::CheckpointTicket {
            ticket,
            purpose,
            at,
            ..
        } => Some((ticket, purpose, at)),
        _ => None,
    })
    .await;
    assert_eq!(purpose, protocol::live::TicketPurpose::Verify);
    assert_eq!(verify_at, at);
    let digest = sha2::Sha256::digest(&file)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let hashed = server
        .http(Some(&bob))
        .post(server.url(&format!("/api/docs/{}/live-checkpoint/hash", doc.0)))
        .json(&serde_json::json!({ "ticket": verify, "at": at.0, "sha256": digest }))
        .send()
        .await
        .unwrap();
    assert_eq!(hashed.status(), StatusCode::ACCEPTED);
    let loaded = server.db.db.load_live(doc).await.unwrap();
    assert_eq!(loaded.verified_through, at);
    let verified = loaded.verified_checkpoint.unwrap();
    assert_eq!(verified.revision, revision);
    assert_eq!(
        verified.session_data, b"ids",
        "the base carries its session data"
    );
}

#[tokio::test]
async fn checkpoint_identity_data_counts_against_the_site_quota() {
    let server = spawn_server_config(|config| {
        live_config(config);
        config.live_checkpoint_quiet = Duration::from_millis(50);
    })
    .await;
    let (doc, ann, _bob) = shared_live(&server).await;
    let root = server.admin("Root").await;
    let site = server.db.db.node(doc.into()).await.unwrap().unwrap().site;
    let used = server.db.db.site_usage(site).await.unwrap().used;
    let response = server
        .http(Some(&root))
        .put(server.url(&format!("/api/sites/{}/quota", site.0)))
        .json(&serde_json::json!({ "bytes": used + 1_500 }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut socket =
        join_capable_socket(&server, doc, &ann, protocol::live::LiveBuild::Browser).await;
    let identities = vec![b'i'; 1_000];
    for local in 1..=2_u64 {
        send_live(
            &mut socket,
            &LiveClientMsg::Entry {
                local: LocalNo(local),
                seen: Seq(local - 1),
                format: FORMAT.to_owned(),
                payload: "x".to_owned(),
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
        let mut query = vec![("ticket", ticket.to_string()), ("at", at.0.to_string())];
        if let Some(base) = &base {
            query.push(("base", base.clone()));
        }
        let body = [identities.as_slice(), format!("book {local}").as_bytes()].concat();
        // The second body has no declared length, so only the reservation can refuse it.
        let body = if local == 1 {
            reqwest::Body::from(body)
        } else {
            reqwest::Body::wrap_stream(futures_util::stream::iter([Ok::<_, std::io::Error>(body)]))
        };
        let response = server
            .http(Some(&ann))
            .put(server.url(&format!("/api/docs/{}/live-checkpoint", doc.0)))
            .query(&query)
            .header("x-sharecraft-session-data-length", identities.len())
            .body(body)
            .send()
            .await
            .unwrap();
        if local == 1 {
            assert_eq!(response.status(), StatusCode::OK);
            let usage = server.db.db.site_usage(site).await.unwrap();
            assert!(
                usage.used >= used + 1_000,
                "stored identity data is used space"
            );
        } else {
            assert_eq!(
                response.status(),
                StatusCode::INSUFFICIENT_STORAGE,
                "the second checkpoint's identity data does not fit the reservation"
            );
        }
    }
}

/// The identity-data limit is checked from the header, before any of the body is read.
#[tokio::test]
async fn session_data_limit_is_enforced_before_upload() {
    let server = spawn_server_config(|config| {
        live_config(config);
        config.live_checkpoint_quiet = Duration::from_millis(50);
    })
    .await;
    let (doc, ann, _bob) = shared_live(&server).await;
    let mut socket =
        join_capable_socket(&server, doc, &ann, protocol::live::LiveBuild::Browser).await;
    send_live(
        &mut socket,
        &LiveClientMsg::Entry {
            local: LocalNo(1),
            seen: Seq(0),
            format: FORMAT.to_owned(),
            payload: "x".to_owned(),
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
    let mut query = vec![("ticket", ticket.to_string()), ("at", at.0.to_string())];
    if let Some(base) = &base {
        query.push(("base", base.clone()));
    }
    let never_finishes = futures_util::stream::pending::<Result<Vec<u8>, std::io::Error>>();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        server
            .http(Some(&ann))
            .put(server.url(&format!("/api/docs/{}/live-checkpoint", doc.0)))
            .query(&query)
            .header("x-sharecraft-session-data-length", 8 * 1024 * 1024 + 1)
            .body(reqwest::Body::wrap_stream(never_finishes))
            .send(),
    )
    .await
    .expect("refused from the header, without reading the body")
    .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

fn sha_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A checkpoint ticket as sent to one of two sockets.
struct Asked {
    /// Whether it went to the first socket.
    first: bool,
    ticket: uuid::Uuid,
    at: Seq,
    base: Option<String>,
    major: Option<protocol::live::MajorReason>,
}

/// The next ticket for `purpose` sent to either socket.
async fn ticket_to_either(
    first: &mut common::Socket,
    second: &mut common::Socket,
    purpose: protocol::live::TicketPurpose,
) -> Asked {
    let pick = move |message: LiveServerMsg| match message {
        LiveServerMsg::CheckpointTicket {
            ticket,
            purpose: sent,
            at,
            base,
            major,
        } if sent == purpose => Some((ticket, at, base, major)),
        _ => None,
    };
    let (first, (ticket, at, base, major)) = tokio::select! {
        found = next_live(first, pick) => (true, found),
        found = next_live(second, pick) => (false, found),
    };
    Asked {
        first,
        ticket,
        at,
        base,
        major,
    }
}

/// Uploads `session_data` then `file` as the checkpoint `asked` names.
async fn upload_checkpoint(
    site: &Site,
    user: &TestUser,
    doc: DocId,
    asked: &Asked,
    session_data: &[u8],
    file: &[u8],
) -> reqwest::Response {
    let mut query = vec![
        ("ticket", asked.ticket.to_string()),
        ("at", asked.at.0.to_string()),
    ];
    if let Some(base) = &asked.base {
        query.push(("base", base.clone()));
    }
    site.http(Some(user))
        .put(site.url(&format!("/api/docs/{}/live-checkpoint", doc.0)))
        .query(&query)
        .header("x-sharecraft-session-data-length", session_data.len())
        .body([session_data, file].concat())
        .send()
        .await
        .unwrap()
}

async fn saved_revision(response: reqwest::Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<serde_json::Value>().await.unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Verifies the checkpoint `asked` names with the SHA-256 of `file`.
async fn verify_checkpoint(site: &Site, user: &TestUser, doc: DocId, asked: &Asked, file: &[u8]) {
    let response = site
        .http(Some(user))
        .post(site.url(&format!("/api/docs/{}/live-checkpoint/hash", doc.0)))
        .json(&serde_json::json!({
            "ticket": asked.ticket,
            "at": asked.at.0,
            "sha256": sha_hex(file),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

/// Saves `content` as a new version of `doc` made from `base` through a resumable upload, as
/// someone outside the live session would. Answers the revision written.
async fn upload_new_version(
    site: &Site,
    user: &TestUser,
    doc: DocId,
    base: &str,
    content: &[u8],
) -> String {
    let mut tries = 0;
    let started = loop {
        let started = site
            .http(Some(user))
            .post(site.url("/api/uploads"))
            .json(&serde_json::json!({
                "doc": doc.0.to_string(),
                "size": content.len(),
                "sha256": sha_hex(content),
                "base": base,
            }))
            .send()
            .await
            .unwrap();
        // No file session opens while the live room is still ending.
        if started.status() != StatusCode::SERVICE_UNAVAILABLE || tries == 100 {
            break started;
        }
        tries += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(started.status(), StatusCode::CREATED);
    let id = started.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sent = site
        .http(Some(user))
        .put(site.url(&format!("/api/uploads/{id}")))
        .header(
            "content-range",
            format!("bytes 0-{}/{}", content.len() - 1, content.len()),
        )
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), StatusCode::OK);
    let finished = site
        .http(Some(user))
        .post(site.url(&format!("/api/uploads/{id}/finish")))
        .send()
        .await
        .unwrap();
    saved_revision(finished).await
}

async fn send_change(socket: &mut common::Socket, local: u64, seen: u64, payload: &str) {
    send_live(
        socket,
        &LiveClientMsg::Entry {
            local: LocalNo(local),
            seen: Seq(seen),
            format: FORMAT.to_owned(),
            payload: payload.to_owned(),
        },
    )
    .await;
}

/// S4 end to end: a verified live checkpoint goes stale when someone uploads a new version
/// from outside while the log after it is unsaved. When the editors come back, the room's last
/// state is kept as a major version below the outside file, the outside file is stored again
/// on top as current and becomes the verified base, everyone reloads it, and checkpointing
/// resumes on top of it. Nothing is merged and nothing is lost.
///
/// While the live room is open no file session can open, so an upload of a new version
/// waits for the room to end; the outside file lands between two live sessions.
#[tokio::test]
async fn stale_live_checkpoint_preserves_both_versions() {
    use protocol::live::{LiveBuild, MajorReason, TicketPurpose};
    let server = spawn_server_config(|config| {
        live_config(config);
        config.live_checkpoint_quiet = Duration::from_millis(50);
        config.room_grace = Duration::from_millis(100);
    })
    .await;
    let (doc, ann, bob) = shared_live(&server).await;
    let mut ann_socket = join_capable_socket(&server, doc, &ann, LiveBuild::Browser).await;
    let mut bob_socket = join_capable_socket(&server, doc, &bob, LiveBuild::Desktop).await;
    let holder = |asked: &Asked| if asked.first { &ann } else { &bob };

    // A verified checkpoint at 1.
    send_change(&mut ann_socket, 1, 0, "one").await;
    let write = ticket_to_either(&mut ann_socket, &mut bob_socket, TicketPurpose::Write).await;
    assert_eq!(write.at, Seq(1));
    let first_file = b"live workbook at one".to_vec();
    let checkpoint = saved_revision(
        upload_checkpoint(&server, holder(&write), doc, &write, b"ids-1", &first_file).await,
    )
    .await;
    let verify = ticket_to_either(&mut ann_socket, &mut bob_socket, TicketPurpose::Verify).await;
    assert_ne!(
        verify.first, write.first,
        "the writer never verifies its own"
    );
    verify_checkpoint(&server, holder(&verify), doc, &verify, &first_file).await;
    assert_eq!(
        server.db.db.load_live(doc).await.unwrap().verified_through,
        Seq(1)
    );

    // A change after it is stored but never checkpointed: everyone leaves first.
    send_change(&mut ann_socket, 2, 1, "two").await;
    next_live(&mut ann_socket, |message| match message {
        LiveServerMsg::Entry { entry } if entry.seq == Seq(2) => Some(()),
        _ => None,
    })
    .await;
    drop(ann_socket);
    drop(bob_socket);

    // Someone saves another file over the live checkpoint once the room has ended.
    let outside_file = b"outside workbook".to_vec();
    let outside = upload_new_version(&server, &ann, doc, &checkpoint, &outside_file).await;
    assert_eq!(
        server.db.db.load_live(doc).await.unwrap().verified_through,
        Seq(1),
        "the unsaved log is kept"
    );

    // The editors come back to a room that finds the outside file newer than its base.
    let mut ann_socket = rejoin_capable(&server, doc, &ann, LiveBuild::Browser).await;
    let mut bob_socket = rejoin_capable(&server, doc, &bob, LiveBuild::Desktop).await;

    // The room asks for its last state once more, as a major version, and changes wait.
    let recovery = ticket_to_either(&mut ann_socket, &mut bob_socket, TicketPurpose::Write).await;
    assert_eq!(
        (recovery.at, recovery.major),
        (Seq(2), Some(MajorReason::OutsideReplacement))
    );
    send_change(&mut bob_socket, 1, 2, "held").await;
    let held = next_live(&mut bob_socket, |message| match message {
        LiveServerMsg::Refused { local, reason } => Some((local, reason)),
        _ => None,
    })
    .await;
    assert_eq!(held, (LocalNo(1), RefusedReason::Held));

    let last_state = b"room's last state at two".to_vec();
    let kept = saved_revision(
        upload_checkpoint(
            &server,
            holder(&recovery),
            doc,
            &recovery,
            b"ids-2",
            &last_state,
        )
        .await,
    )
    .await;

    // Both versions are in the history: the live state below, the outside file on top.
    let history = server.store().history(doc).unwrap();
    let newest: Vec<_> = history.iter().rev().take(3).collect();
    let current = newest[0].revision.to_string();
    assert_eq!(newest[1].revision.to_string(), kept);
    assert_eq!(newest[2].revision.to_string(), outside);
    assert_eq!(
        sha_hex_of(&newest[1].blob.unwrap().sha256),
        sha_hex(&last_state)
    );
    assert_eq!(
        newest[1].fields.get("major"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        newest[1].fields.get("major_reason"),
        Some(&serde_json::json!("outside_replacement"))
    );
    assert_eq!(
        newest[0].fields.get("major_reason"),
        Some(&serde_json::json!("outside_current"))
    );
    assert_eq!(
        newest[0].blob.unwrap().sha256,
        newest[2].blob.unwrap().sha256,
        "the outside file is stored again unchanged"
    );
    assert!(
        history
            .iter()
            .any(|info| info.revision.to_string() == checkpoint),
        "the stale live checkpoint stays in the history"
    );
    let downloaded = server
        .http(Some(&bob))
        .get(server.url(&format!("/api/docs/{}/content", doc.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(downloaded.bytes().await.unwrap().as_ref(), outside_file);

    // The outside file is the verified base, recorded twice as a major version.
    let loaded = server.db.db.load_live(doc).await.unwrap();
    assert_eq!(loaded.verified_through, Seq(2));
    assert_eq!(loaded.verified_checkpoint.unwrap().revision, current);
    wait_events(&server.records_db(), doc, |events| {
        events
            .iter()
            .filter(|event| event.kind == "live.major_version")
            .count()
            == 2
    })
    .await;

    // Everyone loads the outside file and changes resume.
    for socket in [&mut ann_socket, &mut bob_socket] {
        let catchup = next_live(socket, |message| match message {
            LiveServerMsg::Catchup { base, revision, .. } => Some((base, revision)),
            _ => None,
        })
        .await;
        assert_eq!(catchup, (Seq(2), Some(current.clone())));
        next_live(socket, |message| {
            matches!(message, LiveServerMsg::ChangesResumed).then_some(())
        })
        .await;
    }

    // Checkpointing resumes on top of the outside file.
    send_change(&mut ann_socket, 3, 2, "three").await;
    let next = ticket_to_either(&mut ann_socket, &mut bob_socket, TicketPurpose::Write).await;
    assert_eq!(
        (next.at, next.base.as_deref(), next.major),
        (Seq(3), Some(current.as_str()), None)
    );
    saved_revision(
        upload_checkpoint(&server, holder(&next), doc, &next, b"ids-3", b"after three").await,
    )
    .await;
}

/// Joins `doc` as a capable editor, trying again while the room cannot open yet (a file
/// session that took an upload is still ending).
async fn rejoin_capable(
    site: &Site,
    doc: DocId,
    user: &TestUser,
    build: protocol::live::LiveBuild,
) -> common::Socket {
    use futures_util::StreamExt;
    for _ in 0..50 {
        let mut socket = common::open_socket(site.addr, doc, user).await;
        send_live(
            &mut socket,
            &LiveClientMsg::LiveJoin {
                client: collab_core::ids::ClientId(uuid::Uuid::new_v4()),
                protocol: protocol::live::LIVE_PROTOCOL,
                format: FORMAT.to_owned(),
                resume: None,
                seen: None,
                can_checkpoint: true,
                build,
                returning_through: None,
            },
        )
        .await;
        let joined = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match socket.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if matches!(
                            serde_json::from_str(&text).unwrap(),
                            LiveServerMsg::LiveJoined { .. }
                        ) {
                            return true;
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        if joined {
            return socket;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the live room never opened again");
}

fn sha_hex_of(sha256: &[u8; 32]) -> String {
    sha256.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A verified checkpoint outlives the room and the server: after a restart a newcomer is
/// caught up from it (not from the start of the log) and the next checkpoint is made on it.
#[tokio::test]
async fn verified_checkpoint_survives_room_and_server_restart() {
    use protocol::live::{LiveBuild, TicketPurpose};
    let quick = |config: &mut server::Config| {
        live_config(config);
        config.live_checkpoint_quiet = Duration::from_millis(50);
    };
    let data = tempdir().unwrap();
    let db = records::test_support::TestDb::create().await;
    let mut first = StoppableServer::start(data.path(), &db, quick);
    let doc = new_doc();
    let ann = first.user("Ann").await;
    first.create_file_document(doc, &ann).await;
    let bob = first.user("Bob").await;
    share(&first, &ann, doc, &bob, "editor").await;
    let mut ann_socket = join_capable_socket(&first, doc, &ann, LiveBuild::Browser).await;
    let mut bob_socket = join_capable_socket(&first, doc, &bob, LiveBuild::Desktop).await;
    let holder = |asked: &Asked| if asked.first { &ann } else { &bob };
    send_change(&mut ann_socket, 1, 0, "one").await;
    send_change(&mut ann_socket, 2, 1, "two").await;
    let write = loop {
        let asked = ticket_to_either(&mut ann_socket, &mut bob_socket, TicketPurpose::Write).await;
        if asked.at == Seq(2) {
            break asked;
        }
    };
    let file = b"live workbook at two".to_vec();
    let checkpoint = saved_revision(
        upload_checkpoint(&first, holder(&write), doc, &write, b"ids at two", &file).await,
    )
    .await;
    let verify = ticket_to_either(&mut ann_socket, &mut bob_socket, TicketPurpose::Verify).await;
    verify_checkpoint(&first, holder(&verify), doc, &verify, &file).await;
    send_change(&mut ann_socket, 3, 2, "three").await;
    next_live(&mut ann_socket, |message| match message {
        LiveServerMsg::Entry { entry } if entry.seq == Seq(3) => Some(()),
        _ => None,
    })
    .await;
    drop(ann_socket);
    drop(bob_socket);
    first.stop();

    let second = StoppableServer::start(data.path(), &db, quick);
    let cy = second.user("Cy").await;
    share(&second, &ann, doc, &cy, "editor").await;
    let mut socket = common::open_socket(second.addr, doc, &cy).await;
    send_live(
        &mut socket,
        &LiveClientMsg::LiveJoin {
            client: collab_core::ids::ClientId(uuid::Uuid::new_v4()),
            protocol: protocol::live::LIVE_PROTOCOL,
            format: FORMAT.to_owned(),
            resume: None,
            seen: None,
            can_checkpoint: true,
            build: LiveBuild::Desktop,
            returning_through: None,
        },
    )
    .await;
    let catchup = next_live(&mut socket, |message| match message {
        LiveServerMsg::Catchup {
            base,
            revision,
            from,
            head,
        } => Some((base, revision, from, head)),
        _ => None,
    })
    .await;
    assert_eq!(catchup, (Seq(2), Some(checkpoint.clone()), Seq(2), Seq(3)));
    let loaded = db.db.load_live(doc).await.unwrap();
    assert_eq!(loaded.verified_through, Seq(2));
    assert_eq!(
        loaded.verified_checkpoint.unwrap().session_data,
        b"ids at two",
        "the base keeps its identity data"
    );
    let caught_up = next_live(&mut socket, |message| match message {
        LiveServerMsg::Entry { entry } => Some(entry.seq),
        _ => None,
    })
    .await;
    assert_eq!(caught_up, Seq(3), "only the suffix after the base is sent");
    let ticket = next_live(&mut socket, |message| match message {
        LiveServerMsg::CheckpointTicket {
            purpose: TicketPurpose::Write,
            at,
            base,
            ..
        } => Some((at, base)),
        _ => None,
    })
    .await;
    assert_eq!(ticket, (Seq(3), Some(checkpoint)));
}
