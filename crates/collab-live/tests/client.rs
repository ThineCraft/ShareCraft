//! The live client's agreed and shown states: showing, confirming, refusing, going back and
//! re-running, with the stand-in document and hand-written server frames.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::ids::{ClientId, Seq};
use collab_live::client::{
    ClientError, Crash, EntryView, Event, LiveClient, LiveConfig, LiveEditor, LocalError, Mode,
    Outcome, Phase,
};
use collab_live::ids::LiveId;
use collab_live::stand_in::{StandIn, StandInOp};
use protocol::live::{LocalNo, ParticipantNo, RefusedReason};
use serde_json::{Value, json};

const FORMAT: &str = "stand-in/1";

/// Another person's client.
fn other(n: u128) -> ClientId {
    serde_json::from_value(json!(format!("00000000-0000-0000-0000-{n:012x}"))).unwrap()
}

fn joined(participant: u32) -> String {
    json!({
        "type": "live_joined",
        "session": "00000000-0000-0000-0000-0000000000aa",
        "participant": participant,
        "place": { "is": "new" },
        "can_edit": true,
        "heartbeat_ms": 60_000,
        "people": [],
    })
    .to_string()
}

fn empty_catchup() -> String {
    json!({ "type": "catchup", "base": 0, "revision": null, "from": 0, "head": 0 }).to_string()
}

/// A change entry from the server.
#[allow(clippy::too_many_arguments)]
fn entry_full(
    seq: u64,
    participant: u32,
    client: ClientId,
    local: u64,
    seen: u64,
    payload: &str,
    at_ms: i64,
    seed: u32,
) -> String {
    json!({
        "type": "entry",
        "entry": {
            "seq": seq,
            "at_ms": at_ms,
            "seed": seed,
            "kind": "change",
            "participant": participant,
            "client": client,
            "local": local,
            "seen": seen,
            "payload": payload,
            "cancelled": false,
        }
    })
    .to_string()
}

fn entry(
    seq: u64,
    participant: u32,
    client: ClientId,
    local: u64,
    seen: u64,
    payload: &str,
) -> String {
    entry_full(
        seq,
        participant,
        client,
        local,
        seen,
        payload,
        1_000 + seq as i64,
        7,
    )
}

fn insert(id: LiveId, text: &str) -> String {
    StandIn::op(&StandInOp::Insert {
        before: None,
        id,
        text: text.to_owned(),
    })
}

fn set(id: LiveId, text: &str) -> String {
    StandIn::op(&StandInOp::Set {
        id,
        text: text.to_owned(),
    })
}

fn config() -> LiveConfig {
    LiveConfig {
        format: FORMAT.to_owned(),
        crashed_at: None,
    }
}

/// A new client at `participant`, driven into `Live` on an empty document.
fn live<E: LiveEditor>(editor: &mut E, participant: u32) -> LiveClient<E> {
    let mut live = LiveClient::new(config());
    let join = live.connected(0);
    assert_eq!(join.len(), 1);
    live.receive(editor, &joined(participant), 0).unwrap();
    live.receive(editor, &empty_catchup(), 0).unwrap();
    if live.phase() == Phase::LoadingBase {
        live.base_ready(editor, 0).unwrap();
    }
    assert_eq!(live.phase(), Phase::Live);
    live
}

fn sent(step_send: &[String]) -> Vec<Value> {
    step_send
        .iter()
        .map(|frame| serde_json::from_str(frame).unwrap())
        .collect()
}

/// A `StandIn` that counts how often it was sent back to a copy.
struct Counting {
    doc: StandIn,
    restores: usize,
}

impl LiveEditor for Counting {
    type Snapshot = StandIn;

    fn snapshot(&self) -> StandIn {
        self.doc.snapshot()
    }

    fn restore(&mut self, snapshot: &StandIn) {
        self.restores += 1;
        self.doc.restore(snapshot);
    }

    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash> {
        self.doc.apply(entry)
    }
}

#[test]
fn own_change_is_shown_and_sent() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    let step = client
        .local(&mut doc, insert(id, "a"), Mode::ShowAtOnce, 10)
        .unwrap();
    assert_eq!(doc.lines(), vec!["a"]);
    let frames = sent(&step.send);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "entry");
    assert_eq!(frames[0]["local"], 1);
    assert_eq!(frames[0]["seen"], 0);
    assert_eq!(frames[0]["format"], FORMAT);
    assert_eq!(frames[0]["payload"], insert(id, "a"));
    assert_eq!(client.pending(), 1);
}

#[test]
fn confirmation_without_others_does_not_go_back() {
    let mut doc = Counting {
        doc: StandIn::new("a"),
        restores: 0,
    };
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(id, "a"), Mode::ShowAtOnce, 10)
        .unwrap();
    let step = client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(id, "a")),
            20,
        )
        .unwrap();
    assert_eq!(doc.restores, 0);
    assert_eq!(client.pending(), 0);
    assert_eq!(client.agreed_seq(), Seq(1));
    assert_eq!(doc.doc.lines(), vec!["a"]);
    assert!(step.events.iter().any(|event| matches!(
        event,
        Event::Applied {
            seq: Seq(1),
            own: true,
            ..
        }
    )));
}

#[test]
fn others_change_rolls_back_and_reruns() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let mine = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(mine, "a"), Mode::ShowAtOnce, 10)
        .unwrap();
    let step = client
        .receive(
            &mut doc,
            &entry(1, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x")),
            20,
        )
        .unwrap();
    assert_eq!(doc.lines(), vec!["x", "a"]);
    assert_eq!(client.agreed_seq(), Seq(1));
    assert_eq!(client.pending(), 1);
    assert!(step.events.iter().any(|event| matches!(
        event,
        Event::Applied {
            seq: Seq(1),
            own: false,
            ..
        }
    )));
}

#[test]
fn racing_changes_converge() {
    // Both start from a line someone else wrote, then change it at the same moment.
    let base = entry(1, 3, other(3), 1, 0, &insert(LiveId(3, 0), "base"));
    let mut doc_a = StandIn::new("a");
    let mut doc_b = StandIn::new("b");
    let mut a = live(&mut doc_a, 1);
    let mut b = live(&mut doc_b, 2);
    a.receive(&mut doc_a, &base, 5).unwrap();
    b.receive(&mut doc_b, &base, 5).unwrap();

    let line = LiveId(3, 0);
    let a_id = a.new_ids(1).unwrap();
    let b_id = b.new_ids(1).unwrap();
    let a_set = set(line, "from a");
    let a_ins = insert(a_id, "a's line");
    let b_set = set(line, "from b");
    let b_ins = insert(b_id, "b's line");
    a.local(&mut doc_a, a_set.clone(), Mode::ShowAtOnce, 10)
        .unwrap();
    a.local(&mut doc_a, a_ins.clone(), Mode::ShowAtOnce, 10)
        .unwrap();
    b.local(&mut doc_b, b_set.clone(), Mode::ShowAtOnce, 10)
        .unwrap();
    b.local(&mut doc_b, b_ins.clone(), Mode::ShowAtOnce, 10)
        .unwrap();

    // The server's order: A's two, then B's two, all made against seen 1.
    let order = [
        entry(2, 1, a.client(), 1, 1, &a_set),
        entry(3, 1, a.client(), 2, 1, &a_ins),
        entry(4, 2, b.client(), 1, 1, &b_set),
        entry(5, 2, b.client(), 2, 1, &b_ins),
    ];
    for frame in &order {
        a.receive(&mut doc_a, frame, 20).unwrap();
        b.receive(&mut doc_b, frame, 20).unwrap();
    }

    let mut fresh = StandIn::new("c");
    for payload in [insert(LiveId(3, 0), "base"), a_set, a_ins, b_set, b_ins] {
        fresh
            .apply(&EntryView {
                seq: None,
                participant: ParticipantNo(1),
                own: false,
                stamp: None,
                payload: &payload,
            })
            .unwrap();
    }
    assert_eq!(doc_a.lines(), doc_b.lines());
    assert_eq!(doc_a.lines(), fresh.lines());
    assert_eq!(doc_a.lines(), vec!["from b", "a's line", "b's line"]);
    assert_eq!(a.pending(), 0);
    assert_eq!(b.pending(), 0);
    assert_eq!(a.agreed_seq(), Seq(5));
    assert_eq!(b.agreed_seq(), Seq(5));
}

#[test]
fn wait_for_server_is_not_shown_until_confirmed() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    let step = client
        .local(&mut doc, insert(id, "a"), Mode::WaitForServer, 10)
        .unwrap();
    assert!(doc.lines().is_empty());
    assert_eq!(sent(&step.send).len(), 1);
    assert_eq!(client.pending(), 1);
    client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(id, "a")),
            20,
        )
        .unwrap();
    assert_eq!(doc.lines(), vec!["a"]);
    assert_eq!(client.pending(), 0);
}

#[test]
fn stamped_change_is_reapplied_with_server_stamp() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(id, "a"), Mode::ShowAtOnce, 10)
        .unwrap();
    client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(id, "a")),
            20,
        )
        .unwrap();
    let stamp = StandIn::op(&StandInOp::Stamp { id });
    client
        .local(&mut doc, stamp.clone(), Mode::ShowAtOnce, 30)
        .unwrap();
    assert_eq!(doc.lines(), vec!["local"]);
    client
        .receive(
            &mut doc,
            &entry_full(2, 1, client.client(), 2, 1, &stamp, 1_763_290_000_000, 42),
            40,
        )
        .unwrap();
    assert_eq!(doc.lines(), vec!["1763290000000/42"]);
    assert_eq!(client.pending(), 0);
}

#[test]
fn sixty_four_folds_then_goes_back() {
    let mut doc = Counting {
        doc: StandIn::new("a"),
        restores: 0,
    };
    let mut client = live(&mut doc, 1);
    let mut payloads = Vec::new();
    for _ in 0..65 {
        let id = client.new_ids(1).unwrap();
        let payload = insert(id, "line");
        client
            .local(&mut doc, payload.clone(), Mode::ShowAtOnce, 10)
            .unwrap();
        payloads.push(payload);
    }
    for (n, payload) in payloads.iter().take(64).enumerate() {
        let seq = n as u64 + 1;
        client
            .receive(
                &mut doc,
                &entry(seq, 1, client.client(), seq, 0, payload),
                20,
            )
            .unwrap();
    }
    assert_eq!(doc.restores, 1);
    assert_eq!(client.pending(), 1);
    assert_eq!(client.agreed_seq(), Seq(64));
    assert_eq!(doc.doc.lines().len(), 65);
}

#[test]
fn refusal_takes_the_change_back() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let first = client.new_ids(1).unwrap();
    let second = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(first, "one"), Mode::ShowAtOnce, 10)
        .unwrap();
    client
        .local(&mut doc, insert(second, "two"), Mode::ShowAtOnce, 10)
        .unwrap();
    assert_eq!(doc.lines(), vec!["one", "two"]);
    let refused = json!({ "type": "refused", "local": 1, "reason": "too_large" }).to_string();
    let step = client.receive(&mut doc, &refused, 20).unwrap();
    assert_eq!(doc.lines(), vec!["two"]);
    assert_eq!(client.pending(), 1);
    assert!(step.events.contains(&Event::Refused {
        local: LocalNo(1),
        reason: RefusedReason::TooLarge,
    }));
}

#[test]
fn losing_edit_rights_drops_waiting_changes() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    client
        .receive(
            &mut doc,
            &entry(1, 2, other(2), 1, 0, &insert(LiveId(2, 0), "agreed")),
            5,
        )
        .unwrap();
    for text in ["one", "two"] {
        let id = client.new_ids(1).unwrap();
        client
            .local(&mut doc, insert(id, text), Mode::ShowAtOnce, 10)
            .unwrap();
    }
    assert_eq!(doc.lines(), vec!["agreed", "one", "two"]);
    let access = json!({ "type": "access", "can_edit": false }).to_string();
    let step = client.receive(&mut doc, &access, 20).unwrap();
    assert!(step.events.contains(&Event::DroppedPending { count: 2 }));
    assert!(
        step.events
            .contains(&Event::AccessChanged { can_edit: false })
    );
    assert_eq!(doc.lines(), vec!["agreed"]);
    assert_eq!(client.pending(), 0);
    assert!(!client.can_edit());
    let id = client.new_ids(1).unwrap();
    assert_eq!(
        client
            .local(&mut doc, insert(id, "three"), Mode::ShowAtOnce, 30)
            .unwrap_err(),
        LocalError::ReadOnly
    );
    assert_eq!(doc.lines(), vec!["agreed"]);
}

#[test]
fn out_of_order_entry_is_an_error() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let error = client
        .receive(
            &mut doc,
            &entry(2, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x")),
            5,
        )
        .unwrap_err();
    assert_eq!(
        error,
        ClientError::OutOfOrder {
            expected: Seq(1),
            got: Seq(2),
        }
    );
    assert!(doc.lines().is_empty());
    assert_eq!(client.agreed_seq(), Seq(0));
}

#[test]
fn own_entry_out_of_turn_is_desync() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let first = client.new_ids(1).unwrap();
    let second = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(first, "one"), Mode::ShowAtOnce, 10)
        .unwrap();
    client
        .local(&mut doc, insert(second, "two"), Mode::ShowAtOnce, 10)
        .unwrap();
    // The server confirms local 2 before local 1.
    let error = client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 2, 0, &insert(second, "two")),
            20,
        )
        .unwrap_err();
    assert!(matches!(error, ClientError::Desync(_)));
    // A refusal for a change that is not first in line is a desync too.
    let refused = json!({ "type": "refused", "local": 2, "reason": "rate_limited" }).to_string();
    assert!(matches!(
        client.receive(&mut doc, &refused, 20).unwrap_err(),
        ClientError::Desync(_)
    ));
}

#[test]
fn oversized_local_change_is_refused() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let error = client
        .local(&mut doc, "x".repeat(262_145), Mode::ShowAtOnce, 10)
        .unwrap_err();
    assert_eq!(error, LocalError::TooLarge);
    assert_eq!(client.pending(), 0);
    let step = client
        .local(&mut doc, "x".repeat(262_144), Mode::ShowAtOnce, 10)
        .unwrap();
    assert_eq!(sent(&step.send).len(), 1);
    assert_eq!(client.pending(), 1);
}

#[test]
fn local_change_before_live_is_refused() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    assert_eq!(
        client
            .local(&mut doc, insert(LiveId(1, 0), "a"), Mode::ShowAtOnce, 0)
            .unwrap_err(),
        LocalError::NotLive
    );
    assert!(doc.lines().is_empty());
}

#[test]
fn crashing_local_change_is_taken_back() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(id, "kept"), Mode::ShowAtOnce, 10)
        .unwrap();
    let crash = StandIn::op(&StandInOp::CrashIf {
        device: "a".to_owned(),
    });
    let error = client
        .local(&mut doc, crash, Mode::ShowAtOnce, 20)
        .unwrap_err();
    assert_eq!(error, LocalError::Crashed);
    assert_eq!(doc.lines(), vec!["kept"]);
    assert_eq!(client.pending(), 1);
}

#[test]
fn malformed_frame_is_an_error() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    assert!(matches!(
        client.receive(&mut doc, "not json", 5).unwrap_err(),
        ClientError::Malformed(_)
    ));
}

/// The server's answer to a join that kept the place `participant`.
fn joined_kept(participant: u32) -> String {
    let mut frame: Value = serde_json::from_str(&joined(participant)).unwrap();
    frame["place"] = json!({ "is": "kept" });
    frame.to_string()
}

fn catchup(base: u64, from: u64, head: u64) -> String {
    json!({ "type": "catchup", "base": base, "revision": null, "from": from, "head": head })
        .to_string()
}

#[test]
fn refusal_after_access_loss_is_ignored() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    for text in ["one", "two"] {
        let id = client.new_ids(1).unwrap();
        client
            .local(&mut doc, insert(id, text), Mode::ShowAtOnce, 10)
            .unwrap();
    }
    let access = json!({ "type": "access", "can_edit": false }).to_string();
    client.receive(&mut doc, &access, 20).unwrap();
    // The server refused both before it saw the role change reach the client.
    for local in [1, 2] {
        let refused =
            json!({ "type": "refused", "local": local, "reason": "read_only" }).to_string();
        let step = client.receive(&mut doc, &refused, 30).unwrap();
        assert_eq!(step, Default::default());
    }
    assert_eq!(client.phase(), Phase::Live);
    assert!(doc.lines().is_empty());
    // A refusal of a number this client never gave out is still out of step.
    let never = json!({ "type": "refused", "local": 3, "reason": "read_only" }).to_string();
    assert!(matches!(
        client.receive(&mut doc, &never, 40).unwrap_err(),
        ClientError::Desync(_)
    ));
}

#[test]
fn fresh_clients_get_different_ids() {
    let first: LiveClient<StandIn> = LiveClient::new(config());
    let second: LiveClient<StandIn> = LiveClient::new(config());
    assert_ne!(first.client(), second.client());
    assert_eq!(first.resume(), None);
    // The join frame names the client's own identifier and no place.
    let mut third: LiveClient<StandIn> = LiveClient::new(config());
    let join: Value = serde_json::from_str(&third.connected(0)[0]).unwrap();
    assert_eq!(join["client"], json!(third.client()));
    assert_eq!(join["resume"], Value::Null);
    assert_eq!(join["seen"], Value::Null);
}

#[test]
fn reconnect_keeps_local_numbers_and_ids() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let first = client.new_ids(1).unwrap();
    assert_eq!(first, LiveId(1, 0));
    client
        .local(&mut doc, insert(first, "one"), Mode::ShowAtOnce, 10)
        .unwrap();

    client.disconnected(20);
    let join: Value = serde_json::from_str(&client.connected(30)[0]).unwrap();
    assert_eq!(join["client"], json!(client.client()));
    assert_eq!(join["resume"]["participant"], 1);
    assert_eq!(join["seen"], 0);
    client.receive(&mut doc, &joined_kept(1), 40).unwrap();
    let step = client.receive(&mut doc, &catchup(0, 0, 0), 40).unwrap();
    assert_eq!(client.phase(), Phase::Live);
    // The unconfirmed change goes again under its own number.
    let resent = sent(&step.send);
    assert_eq!(resent.len(), 1);
    assert_eq!(resent[0]["local"], 1);

    // New work carries on from where the client was.
    let second = client.new_ids(1).unwrap();
    assert_eq!(second, LiveId(1, 1));
    let step = client
        .local(&mut doc, insert(second, "two"), Mode::ShowAtOnce, 50)
        .unwrap();
    assert_eq!(sent(&step.send)[0]["local"], 2);
    assert_eq!(doc.lines(), vec!["one", "two"]);
}

#[test]
fn own_confirmation_with_other_content_is_desync() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(id, "mine"), Mode::ShowAtOnce, 10)
        .unwrap();
    let error = client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(id, "not mine")),
            20,
        )
        .unwrap_err();
    assert!(matches!(error, ClientError::Desync(_)));
    assert_eq!(client.pending(), 1);
}

#[test]
fn others_entry_lands_after_folded_own_entries() {
    let mut doc = Counting {
        doc: StandIn::new("a"),
        restores: 0,
    };
    let mut client = live(&mut doc, 1);
    let a = client.new_ids(1).unwrap();
    let b = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(a, "a"), Mode::ShowAtOnce, 10)
        .unwrap();
    client
        .local(&mut doc, insert(b, "b"), Mode::ShowAtOnce, 10)
        .unwrap();
    // "a" is confirmed and folded; "b" still waits.
    client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(a, "a")),
            20,
        )
        .unwrap();
    assert_eq!(doc.restores, 0);
    client
        .receive(
            &mut doc,
            &entry(2, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x")),
            30,
        )
        .unwrap();
    assert_eq!(doc.doc.lines(), vec!["a", "x", "b"]);
    assert_eq!(doc.restores, 1);
    assert_eq!(client.agreed_seq(), Seq(2));
    assert_eq!(client.pending(), 1);
}

#[test]
fn own_earlier_entry_met_again_from_the_base_is_applied() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let one = client.new_ids(1).unwrap();
    let two = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(one, "one"), Mode::ShowAtOnce, 10)
        .unwrap();
    client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(one, "one")),
            20,
        )
        .unwrap();
    client
        .local(&mut doc, insert(two, "two"), Mode::ShowAtOnce, 30)
        .unwrap();

    // After a drop the server sends the log from the base again.
    client.disconnected(40);
    client.connected(50);
    client.receive(&mut doc, &joined_kept(1), 50).unwrap();
    let step = client.receive(&mut doc, &catchup(0, 0, 1), 50).unwrap();
    assert!(step.events.contains(&Event::LoadBase {
        revision: None,
        at: Seq(0),
    }));
    client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &insert(one, "one")),
            60,
        )
        .unwrap();
    doc = StandIn::new("a");
    let step = client.base_ready(&mut doc, 70).unwrap();
    assert!(step.events.iter().any(|event| matches!(
        event,
        Event::Applied {
            seq: Seq(1),
            own: true,
            ..
        }
    )));
    assert_eq!(client.phase(), Phase::Live);
    assert_eq!(client.pending(), 1);
    assert_eq!(client.agreed_seq(), Seq(1));
    assert_eq!(doc.lines(), vec!["one", "two"]);
    let resent = sent(&step.send);
    assert_eq!(resent.len(), 1);
    assert_eq!(resent[0]["local"], 2);
    assert_eq!(resent[0]["seen"], 1);
}

// ---- Task 5: joining, resending, the heartbeat and pacing ----

fn joined_as(participant: u32, place: Value, can_edit: bool) -> String {
    let mut frame: Value = serde_json::from_str(&joined(participant)).unwrap();
    frame["place"] = place;
    frame["can_edit"] = json!(can_edit);
    frame.to_string()
}

/// The entry frames among `frames`.
fn entries(frames: &[String]) -> Vec<Value> {
    sent(frames)
        .into_iter()
        .filter(|frame| frame["type"] == "entry")
        .collect()
}

fn heartbeats(frames: &[String]) -> Vec<String> {
    frames
        .iter()
        .filter(|frame| frame.contains("\"heartbeat\""))
        .cloned()
        .collect()
}

/// A `StandIn` that records who each applied change was shown as coming from.
struct Recording {
    doc: StandIn,
    applied: Vec<(Option<Seq>, ParticipantNo, bool)>,
}

impl LiveEditor for Recording {
    type Snapshot = StandIn;

    fn snapshot(&self) -> StandIn {
        self.doc.snapshot()
    }

    fn restore(&mut self, snapshot: &StandIn) {
        self.doc.restore(snapshot);
    }

    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash> {
        self.applied.push((entry.seq, entry.participant, entry.own));
        self.doc.apply(entry)
    }
}

#[test]
fn join_from_nothing_loads_the_base() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    let join: Value = serde_json::from_str(&client.connected(0)[0]).unwrap();
    assert_eq!(join["type"], "live_join");
    assert_eq!(join["protocol"], 1);
    assert_eq!(join["format"], FORMAT);
    assert_eq!(client.phase(), Phase::Joining);
    client.receive(&mut doc, &joined(1), 0).unwrap();
    let step = client.receive(&mut doc, &catchup(0, 0, 2), 0).unwrap();
    assert_eq!(
        step.events,
        vec![Event::LoadBase {
            revision: None,
            at: Seq(0),
        }]
    );
    assert_eq!(client.phase(), Phase::LoadingBase);
    client.base_ready(&mut doc, 10).unwrap();
    assert_eq!(client.phase(), Phase::CatchingUp);
    client
        .receive(
            &mut doc,
            &entry(1, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x")),
            20,
        )
        .unwrap();
    assert_eq!(client.phase(), Phase::CatchingUp);
    client
        .receive(
            &mut doc,
            &entry(2, 2, other(2), 2, 1, &insert(LiveId(2, 1), "y")),
            20,
        )
        .unwrap();
    assert_eq!(client.phase(), Phase::Live);
    assert_eq!(client.agreed_seq(), Seq(2));
    assert_eq!(doc.lines(), vec!["x", "y"]);
}

#[test]
fn rejoin_with_kept_place_catches_up_from_seen() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    for seq in 1..=3u64 {
        client
            .receive(
                &mut doc,
                &entry(
                    seq,
                    2,
                    other(2),
                    seq,
                    seq - 1,
                    &insert(LiveId(2, seq as u32), "x"),
                ),
                10,
            )
            .unwrap();
    }
    client.disconnected(20);
    assert_eq!(client.phase(), Phase::Offline);
    let join: Value = serde_json::from_str(&client.connected(30)[0]).unwrap();
    assert_eq!(join["seen"], 3);
    assert_eq!(join["resume"]["participant"], 1);
    client.receive(&mut doc, &joined_kept(1), 30).unwrap();
    let step = client.receive(&mut doc, &catchup(0, 3, 4), 30).unwrap();
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::LoadBase { .. }))
    );
    assert_eq!(client.phase(), Phase::CatchingUp);
    client
        .receive(
            &mut doc,
            &entry(4, 2, other(2), 4, 3, &insert(LiveId(2, 9), "y")),
            40,
        )
        .unwrap();
    assert_eq!(client.phase(), Phase::Live);
    assert_eq!(doc.lines(), vec!["x", "x", "x", "y"]);
}

#[test]
fn reconnect_resends_only_unconfirmed() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let mut payloads = Vec::new();
    for text in ["one", "two", "three"] {
        let id = client.new_ids(1).unwrap();
        let payload = insert(id, text);
        let step = client
            .local(&mut doc, payload.clone(), Mode::ShowAtOnce, 10)
            .unwrap();
        assert_eq!(entries(&step.send).len(), 1);
        payloads.push(payload);
    }
    client.disconnected(20);
    client.connected(30);
    client.receive(&mut doc, &joined_kept(1), 30).unwrap();
    let step = client.receive(&mut doc, &catchup(0, 0, 1), 30).unwrap();
    assert!(step.send.is_empty(), "nothing goes before the head");
    assert_eq!(client.tick(31), Vec::<String>::new());
    // The server stored local 1 before the drop.
    let step = client
        .receive(
            &mut doc,
            &entry(1, 1, client.client(), 1, 0, &payloads[0]),
            40,
        )
        .unwrap();
    assert_eq!(client.phase(), Phase::Live);
    let resent = entries(&step.send);
    assert_eq!(resent.len(), 2);
    assert_eq!(resent[0]["local"], 2);
    assert_eq!(resent[0]["payload"], payloads[1]);
    assert_eq!(resent[1]["local"], 3);
    assert_eq!(resent[1]["payload"], payloads[2]);
    assert_eq!(client.pending(), 2);
    assert_eq!(doc.lines(), vec!["one", "two", "three"]);
    // Nothing is sent twice on the same connection.
    assert!(entries(&client.tick(5_000)).is_empty());
}

#[test]
fn returning_client_keeps_waiting_changes() {
    let mut doc = Recording {
        doc: StandIn::new("a"),
        applied: Vec::new(),
    };
    let mut client = live(&mut doc, 2);
    let first = client.new_ids(1).unwrap();
    assert_eq!(first, LiveId(2, 0));
    client
        .local(&mut doc, insert(first, "one"), Mode::ShowAtOnce, 10)
        .unwrap();
    client
        .receive(
            &mut doc,
            &entry(1, 3, other(3), 1, 0, &insert(LiveId(3, 0), "x")),
            20,
        )
        .unwrap();
    let second = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(second, "two"), Mode::ShowAtOnce, 30)
        .unwrap();

    // The place expired while the client was away.
    client.disconnected(40);
    let join: Value = serde_json::from_str(&client.connected(200_000)[0]).unwrap();
    assert_eq!(join["resume"]["participant"], 2);
    let mark = doc.applied.len();
    let step = client
        .receive(
            &mut doc,
            &joined_as(5, json!({ "is": "returning", "previous": 2 }), true),
            200_000,
        )
        .unwrap();
    assert!(step.events.contains(&Event::Returned {
        previous: ParticipantNo(2),
        now: ParticipantNo(5),
    }));
    assert_eq!(client.participant(), Some(ParticipantNo(5)));
    assert_eq!(client.resume().unwrap().participant, ParticipantNo(5));
    // The waiting changes are shown again under the new number.
    let rerun: Vec<_> = doc.applied[mark..]
        .iter()
        .filter(|(seq, _, _)| seq.is_none())
        .collect();
    assert_eq!(rerun.len(), 2);
    assert!(
        rerun
            .iter()
            .all(|(_, participant, own)| *participant == ParticipantNo(5) && *own)
    );
    assert_eq!(doc.doc.lines(), vec!["x", "one", "two"]);

    let step = client
        .receive(&mut doc, &catchup(0, 1, 1), 200_000)
        .unwrap();
    assert_eq!(client.phase(), Phase::Live);
    let resent = entries(&step.send);
    assert_eq!(resent.len(), 2);
    assert_eq!(resent[0]["local"], 1);
    assert_eq!(resent[0]["seen"], 0);
    assert_eq!(resent[1]["local"], 2);
    assert_eq!(resent[1]["seen"], 1);
    assert_eq!(client.new_ids(1).unwrap(), LiveId(5, 0));
}

#[test]
fn heartbeat_every_minute_whatever_else_is_sent() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    let start = 1_000;
    client.connected(start);
    client.receive(&mut doc, &joined(1), start).unwrap();
    client.receive(&mut doc, &empty_catchup(), start).unwrap();
    client.base_ready(&mut doc, start).unwrap();
    client
        .receive(
            &mut doc,
            &entry(1, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x")),
            start + 5,
        )
        .unwrap();
    for at in [10_000, 30_000, 59_000] {
        let id = client.new_ids(1).unwrap();
        let step = client
            .local(&mut doc, insert(id, "x"), Mode::ShowAtOnce, start + at)
            .unwrap();
        assert_eq!(entries(&step.send).len(), 1);
        assert!(heartbeats(&client.tick(start + at)).is_empty());
    }
    assert!(heartbeats(&client.tick(start + 59_999)).is_empty());
    let frames = client.tick(start + 60_000);
    assert_eq!(frames, vec![r#"{"type":"heartbeat","seen":1}"#.to_owned()]);
    assert!(client.tick(start + 60_001).is_empty());
    assert!(heartbeats(&client.tick(start + 119_999)).is_empty());
    assert_eq!(heartbeats(&client.tick(start + 120_000)).len(), 1);
}

#[test]
fn sending_is_paced() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let now = 10_000;
    let mut first = Vec::new();
    for _ in 0..50 {
        let id = client.new_ids(1).unwrap();
        let step = client
            .local(&mut doc, insert(id, "x"), Mode::ShowAtOnce, now)
            .unwrap();
        first.extend(entries(&step.send));
    }
    assert_eq!(first.len(), 20);
    assert_eq!(doc.lines().len(), 50, "every change is shown at once");
    assert!(entries(&client.tick(now + 999)).is_empty());
    let second = entries(&client.tick(now + 1_000));
    assert_eq!(second.len(), 20);
    assert!(entries(&client.tick(now + 1_500)).is_empty());
    let third = entries(&client.tick(now + 2_000));
    assert_eq!(third.len(), 10);
    let locals: Vec<u64> = first
        .iter()
        .chain(&second)
        .chain(&third)
        .map(|frame| frame["local"].as_u64().unwrap())
        .collect();
    assert_eq!(locals, (1..=50).collect::<Vec<u64>>());
    assert!(entries(&client.tick(now + 3_000)).is_empty());
}

#[test]
fn viewer_never_sends_entries() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    client.connected(0);
    client
        .receive(&mut doc, &joined_as(1, json!({ "is": "new" }), false), 0)
        .unwrap();
    client.receive(&mut doc, &empty_catchup(), 0).unwrap();
    client.base_ready(&mut doc, 0).unwrap();
    assert_eq!(client.phase(), Phase::Live);
    assert!(!client.can_edit());
    assert_eq!(
        client
            .local(&mut doc, insert(LiveId(1, 0), "a"), Mode::ShowAtOnce, 10)
            .unwrap_err(),
        LocalError::ReadOnly
    );
    let step = client
        .receive(
            &mut doc,
            &entry(1, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x")),
            20,
        )
        .unwrap();
    assert!(step.send.is_empty());
    assert_eq!(doc.lines(), vec!["x"]);
    let frames = client.tick(60_000);
    assert_eq!(frames, vec![r#"{"type":"heartbeat","seen":1}"#.to_owned()]);
}

#[test]
fn rejoining_as_viewer_drops_waiting_changes() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(id, "one"), Mode::ShowAtOnce, 10)
        .unwrap();
    client.disconnected(20);
    client.connected(30);
    // The person became a viewer while the connection was down.
    let step = client
        .receive(&mut doc, &joined_as(1, json!({ "is": "kept" }), false), 30)
        .unwrap();
    assert!(step.events.contains(&Event::DroppedPending { count: 1 }));
    assert_eq!(client.pending(), 0);
    assert!(doc.lines().is_empty());
    let step = client.receive(&mut doc, &catchup(0, 0, 0), 30).unwrap();
    assert_eq!(client.phase(), Phase::Live);
    assert!(step.send.is_empty());
    assert!(entries(&client.tick(5_000)).is_empty());
}

#[test]
fn closed_client_refuses_everything() {
    let last_frames = [
        json!({ "type": "removed", "reason": "access_revoked" }).to_string(),
        json!({ "type": "incompatible", "protocol": 1, "format": "other/1" }).to_string(),
        json!({ "type": "cannot_open", "seq": 1 }).to_string(),
    ];
    for last in &last_frames {
        let mut doc = StandIn::new("a");
        let mut client = live(&mut doc, 1);
        let id = client.new_ids(1).unwrap();
        client
            .local(&mut doc, insert(id, "one"), Mode::ShowAtOnce, 10)
            .unwrap();
        let step = client.receive(&mut doc, last, 20).unwrap();
        assert_eq!(step.events.len(), 1);
        assert!(step.send.is_empty());
        assert_eq!(client.phase(), Phase::Closed);

        assert!(client.connected(30).is_empty());
        assert!(client.tick(1_000_000).is_empty());
        assert_eq!(
            client
                .local(&mut doc, insert(LiveId(1, 5), "two"), Mode::ShowAtOnce, 40)
                .unwrap_err(),
            LocalError::NotLive
        );
        let frame = entry(1, 2, other(2), 1, 0, &insert(LiveId(2, 0), "x"));
        assert_eq!(
            client.receive(&mut doc, &frame, 50).unwrap(),
            Default::default()
        );
        assert_eq!(
            client.receive(&mut doc, "not json", 50).unwrap(),
            Default::default()
        );
        assert!(client.base_ready(&mut doc, 60).is_err());
        assert!(client.new_ids(1).is_err());
        assert_eq!(client.leave(), None);
        client.disconnected(70);
        assert_eq!(client.phase(), Phase::Closed);
        assert_eq!(doc.lines(), vec!["one"]);
    }
}

#[test]
fn leave_sends_one_frame_and_closes() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    assert_eq!(client.leave(), Some(r#"{"type":"leave"}"#.to_owned()));
    assert_eq!(client.phase(), Phase::Closed);
    assert_eq!(client.leave(), None);
    assert!(client.tick(120_000).is_empty());
}

#[test]
fn expired_client_can_join_again() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let id = client.new_ids(1).unwrap();
    client
        .local(&mut doc, insert(id, "one"), Mode::ShowAtOnce, 10)
        .unwrap();
    let step = client
        .receive(&mut doc, &json!({ "type": "expired" }).to_string(), 20)
        .unwrap();
    assert_eq!(step.events, vec![Event::Expired]);
    assert_eq!(client.phase(), Phase::Offline);
    assert!(client.tick(200_000).is_empty());
    let join = client.connected(200_000);
    assert_eq!(join.len(), 1);
    let join: Value = serde_json::from_str(&join[0]).unwrap();
    assert_eq!(join["type"], "live_join");
    assert_eq!(join["resume"]["participant"], 1);
    assert_eq!(join["seen"], 0);
    assert_eq!(client.phase(), Phase::Joining);
    assert_eq!(client.pending(), 1);
}

#[test]
fn incompatible_format_closes() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    client.connected(0);
    let step = client
        .receive(
            &mut doc,
            &json!({ "type": "incompatible", "protocol": 1, "format": "grid/1" }).to_string(),
            0,
        )
        .unwrap();
    assert_eq!(
        step.events,
        vec![Event::Incompatible {
            protocol: 1,
            format: "grid/1".to_owned(),
        }]
    );
    assert_eq!(client.phase(), Phase::Closed);
    assert!(client.connected(10).is_empty());
}

#[test]
fn participant_zero_is_refused() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    client.connected(0);
    assert!(matches!(
        client.receive(&mut doc, &joined(0), 0).unwrap_err(),
        ClientError::Desync(_)
    ));
    assert_eq!(client.participant(), None);
    assert_eq!(client.resume(), None);
    assert!(client.new_ids(1).is_err());
    // Without an accepted join the catch-up is out of step too, so the client never goes live.
    assert!(matches!(
        client.receive(&mut doc, &empty_catchup(), 0).unwrap_err(),
        ClientError::Desync(_)
    ));
    assert_eq!(client.phase(), Phase::Joining);
}

#[test]
fn kept_place_under_another_number_is_desync() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    client.disconnected(10);
    client.connected(20);
    assert!(matches!(
        client.receive(&mut doc, &joined_kept(4), 20).unwrap_err(),
        ClientError::Desync(_)
    ));
    assert_eq!(client.participant(), Some(ParticipantNo(1)));
    assert_eq!(client.new_ids(1).unwrap(), LiveId(1, 0));
}

// ---- Task 5, fix round 1 ----

#[test]
fn clock_going_backwards_does_not_stall() {
    let mut doc = StandIn::new("a");
    let mut client: LiveClient<StandIn> = LiveClient::new(config());
    let start = 100_000;
    client.connected(start);
    client.receive(&mut doc, &joined(1), start).unwrap();
    client.receive(&mut doc, &empty_catchup(), start).unwrap();
    client.base_ready(&mut doc, start).unwrap();
    let mut first = 0;
    for _ in 0..21 {
        let id = client.new_ids(1).unwrap();
        let step = client
            .local(&mut doc, insert(id, "x"), Mode::ShowAtOnce, start)
            .unwrap();
        first += entries(&step.send).len();
    }
    assert_eq!(first, 20);
    // The host's clock jumps back by 95 seconds.
    let frames = client.tick(5_000);
    assert_eq!(entries(&frames).len(), 1, "the pace window starts again");
    assert!(heartbeats(&frames).is_empty());
    assert!(heartbeats(&client.tick(64_999)).is_empty());
    assert_eq!(heartbeats(&client.tick(65_000)).len(), 1);
    assert_eq!(heartbeats(&client.tick(125_000)).len(), 1);
}

#[test]
fn rerun_under_new_number_is_not_folded_as_stored() {
    let mut doc = Recording {
        doc: StandIn::new("a"),
        applied: Vec::new(),
    };
    let mut client = live(&mut doc, 2);
    let id = client.new_ids(1).unwrap();
    let payload = insert(id, "one");
    client
        .local(&mut doc, payload.clone(), Mode::ShowAtOnce, 10)
        .unwrap();
    // The server stored it under participant 2, then the place expired before it was confirmed.
    client.disconnected(20);
    client.connected(200_000);
    client
        .receive(
            &mut doc,
            &joined_as(5, json!({ "is": "returning", "previous": 2 }), true),
            200_000,
        )
        .unwrap();
    client
        .receive(&mut doc, &catchup(0, 0, 1), 200_000)
        .unwrap();
    let mark = doc.applied.len();
    client
        .receive(
            &mut doc,
            &entry(1, 2, client.client(), 1, 0, &payload),
            200_010,
        )
        .unwrap();
    // The stored entry is applied as stored, under participant 2, not folded from the re-run.
    assert!(
        doc.applied[mark..].contains(&(Some(Seq(1)), ParticipantNo(2), true)),
        "applied: {:?}",
        &doc.applied[mark..]
    );
    assert_eq!(client.pending(), 0);
    assert_eq!(client.phase(), Phase::Live);
    assert_eq!(doc.doc.lines(), vec!["one"]);
}

#[test]
fn pace_window_carries_across_a_reconnect() {
    let mut doc = StandIn::new("a");
    let mut client = live(&mut doc, 1);
    let now = 10_000;
    for _ in 0..20 {
        let id = client.new_ids(1).unwrap();
        client
            .local(&mut doc, insert(id, "x"), Mode::ShowAtOnce, now)
            .unwrap();
    }
    client.disconnected(now + 100);
    client.connected(now + 200);
    client
        .receive(&mut doc, &joined_kept(1), now + 200)
        .unwrap();
    let step = client
        .receive(&mut doc, &catchup(0, 0, 0), now + 200)
        .unwrap();
    assert!(entries(&step.send).is_empty(), "the window is still full");
    assert!(entries(&client.tick(now + 999)).is_empty());
    assert_eq!(entries(&client.tick(now + 1_000)).len(), 20);
}

#[test]
fn server_heartbeat_interval_is_clamped() {
    let mut doc = StandIn::new("a");
    for (given, due) in [(0u64, 1_000u64), (500_000, 60_000)] {
        let mut client: LiveClient<StandIn> = LiveClient::new(config());
        client.connected(0);
        let mut frame: Value = serde_json::from_str(&joined(1)).unwrap();
        frame["heartbeat_ms"] = json!(given);
        client.receive(&mut doc, &frame.to_string(), 0).unwrap();
        client.receive(&mut doc, &empty_catchup(), 0).unwrap();
        client.base_ready(&mut doc, 0).unwrap();
        assert!(heartbeats(&client.tick(due - 1)).is_empty());
        assert_eq!(heartbeats(&client.tick(due)).len(), 1);
    }
}
