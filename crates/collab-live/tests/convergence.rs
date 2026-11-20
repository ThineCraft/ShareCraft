//! Random sessions of three live clients against the simulated server: whatever the order
//! frames arrive in, however often connections drop, places expire, editors crash and
//! pages reload, every client ends showing the same document, and it is the server's log
//! applied in order.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/sim.rs"]
mod sim;

use std::collections::{HashSet, VecDeque};

use collab_core::ids::{ClientId, Seq};
use collab_live::client::{
    Crash, EntryView, Event, LiveClient, LiveConfig, LiveEditor, Mode, Outcome, Phase, Stamp, Step,
};
use collab_live::ids::LiveId;
use collab_live::stand_in::{StandIn, StandInOp};
use proptest::prelude::*;
use protocol::live::EntryBody;
use sim::{ConnId, Sim};

const FORMAT: &str = "stand-in/1";
const DEVICES: [&str; 3] = ["a", "b", "c"];
const MAX_CHANGES: usize = 60;

/// A device's editor, whose crashes leave it half changed, and the host's crash note, which
/// survives a reload.
struct Host {
    doc: StandIn,
    note: Option<Seq>,
    /// Entries the editor on this device stopped in, kept across reloads.
    crashed: HashSet<Seq>,
    /// Entries the editor was asked to apply again after stopping in them: the client must
    /// never do that (the stall and the crash note prevent it).
    again: Vec<Seq>,
}

impl LiveEditor for Host {
    type Snapshot = StandIn;

    fn snapshot(&self) -> StandIn {
        self.doc.snapshot()
    }

    fn restore(&mut self, snapshot: &StandIn) {
        self.doc.restore(snapshot);
    }

    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash> {
        if let Some(seq) = entry.seq
            && self.crashed.contains(&seq)
        {
            self.again.push(seq);
        }
        let result = self.doc.apply(entry);
        if result.is_err()
            && let Some(seq) = entry.seq
        {
            self.crashed.insert(seq);
        }
        result
    }

    fn note_applying(&mut self, seq: Option<Seq>) {
        self.note = seq;
    }
}

/// One device: an editor, its client, and the frames it sent that the server has not read.
struct Device {
    name: &'static str,
    doc: Host,
    client: LiveClient<Host>,
    conn: Option<ConnId>,
    to_server: VecDeque<String>,
    now: u64,
    /// Whether the client asked for the base and the host has not loaded it yet.
    loading: bool,
    /// How many changes this client took (its local numbers are 1 to `made`).
    made: u64,
    /// Local numbers the client reported refused.
    refused: Vec<u64>,
    /// Local numbers dropped because the person lost the right to edit.
    dropped: Vec<u64>,
}

impl Device {
    /// A new client on a fresh device.
    fn new(name: &'static str) -> Device {
        Device::reloaded(name, None, HashSet::new(), Vec::new(), 0)
    }

    /// A new client; `note` is the host's crash note, kept across a reload, as are the
    /// records of where the editor crashed.
    fn reloaded(
        name: &'static str,
        note: Option<Seq>,
        crashed: HashSet<Seq>,
        again: Vec<Seq>,
        now: u64,
    ) -> Device {
        Device {
            name,
            doc: Host {
                doc: StandIn::damaging(name),
                note,
                crashed,
                again,
            },
            client: LiveClient::new(LiveConfig {
                format: FORMAT.to_owned(),
                crashed_at: note,
            }),
            conn: None,
            to_server: VecDeque::new(),
            now,
            loading: false,
            made: 0,
            refused: Vec::new(),
            dropped: Vec::new(),
        }
    }
}

struct World {
    sim: Sim,
    devices: Vec<Device>,
    /// Every lasting id any client made, for changes to name.
    ids: Vec<LiveId>,
    changes: usize,
    texts: u64,
    /// Every (client, local) any client, reloaded ones included, reported refused.
    refused: Vec<(ClientId, u64)>,
}

impl World {
    fn new() -> World {
        World {
            sim: Sim::new(FORMAT),
            devices: DEVICES.iter().map(|name| Device::new(name)).collect(),
            ids: Vec::new(),
            changes: 0,
            texts: 0,
            refused: Vec::new(),
        }
    }

    /// Takes what a call to the client produced.
    fn handle(&mut self, at: usize, step: Step) -> Result<(), TestCaseError> {
        let device = &mut self.devices[at];
        device.to_server.extend(step.send);
        for event in step.events {
            match event {
                Event::LoadBase { revision, at: base } => {
                    prop_assert_eq!(revision, None);
                    prop_assert_eq!(base, Seq(0));
                    // The host loads the base later (`base_ready`), so entries may be held.
                    self.devices[at].loading = true;
                }
                Event::Refused { local, .. } => {
                    let device = &mut self.devices[at];
                    device.refused.push(local.0);
                    self.refused.push((device.client.client(), local.0));
                }
                Event::DroppedPending { count } => {
                    // The waiting changes are always the latest ones made.
                    let device = &mut self.devices[at];
                    let first = device.made + 1 - count as u64;
                    device.dropped.extend(first..=device.made);
                }
                Event::Expired => {
                    // The server closed the connection after this last frame.
                    let device = &mut self.devices[at];
                    device.loading = false;
                    if let Some(conn) = device.conn.take() {
                        self.sim.drop_conn(conn);
                    }
                    device.to_server.clear();
                }
                Event::Incompatible { .. } | Event::Removed { .. } | Event::CannotOpen { .. } => {
                    return Err(TestCaseError::fail(format!(
                        "device {at} was shut out: {event:?}"
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn connect(&mut self, at: usize) -> Result<(), TestCaseError> {
        let device = &mut self.devices[at];
        if device.conn.is_some() || device.client.phase() == Phase::Closed {
            return Ok(());
        }
        let conn = self.sim.connect();
        device.conn = Some(conn);
        device.to_server.clear();
        let join = device.client.connected(device.now);
        device.to_server.extend(join);
        Ok(())
    }

    /// The host has loaded the base the client asked for.
    fn base_ready(&mut self, at: usize) -> Result<(), TestCaseError> {
        let device = &mut self.devices[at];
        if !std::mem::take(&mut device.loading) || device.client.phase() != Phase::LoadingBase {
            return Ok(());
        }
        device.doc.doc = StandIn::damaging(device.name);
        let step = device
            .client
            .base_ready(&mut device.doc, device.now)
            .map_err(|error| TestCaseError::fail(format!("base_ready: {error}")))?;
        self.handle(at, step)
    }

    fn disconnect(&mut self, at: usize) {
        let device = &mut self.devices[at];
        device.loading = false;
        if let Some(conn) = device.conn.take() {
            self.sim.drop_conn(conn);
            device.to_server.clear();
            device.client.disconnected(device.now);
        }
    }

    fn deliver_to_server(&mut self, at: usize, count: usize) {
        for _ in 0..count {
            let device = &mut self.devices[at];
            let (Some(conn), Some(text)) = (device.conn, device.to_server.pop_front()) else {
                return;
            };
            self.sim.receive(conn, &text);
        }
    }

    fn deliver_to_client(&mut self, at: usize, count: usize) -> Result<(), TestCaseError> {
        for _ in 0..count {
            let Some(conn) = self.devices[at].conn else {
                return Ok(());
            };
            let Some(text) = self.sim.next_out(conn) else {
                return Ok(());
            };
            let device = &mut self.devices[at];
            let step = device
                .client
                .receive(&mut device.doc, &text, device.now)
                .map_err(|error| {
                    TestCaseError::fail(format!("device {at} could not follow {text}: {error}"))
                })?;
            self.handle(at, step)?;
        }
        Ok(())
    }

    fn tick(&mut self, at: usize, ms: u64) {
        let device = &mut self.devices[at];
        device.now += ms;
        let frames = device.client.tick(device.now);
        if device.conn.is_some() {
            device.to_server.extend(frames);
        }
    }

    fn change(
        &mut self,
        at: usize,
        kind: u8,
        pick: usize,
        wait: bool,
    ) -> Result<(), TestCaseError> {
        if self.changes >= MAX_CHANGES {
            return Ok(());
        }
        let known = (!self.ids.is_empty()).then(|| self.ids[pick % self.ids.len()]);
        self.texts += 1;
        let text = format!("{}{}", DEVICES[at], self.texts);
        let op = match kind {
            0..=4 => {
                let Ok(id) = self.devices[at].client.new_ids(1) else {
                    return Ok(());
                };
                let before = if pick.is_multiple_of(3) { None } else { known };
                self.ids.push(id);
                StandInOp::Insert { before, id, text }
            }
            5 | 6 => match known {
                Some(id) => StandInOp::Set { id, text },
                None => return Ok(()),
            },
            7 => match known {
                Some(id) => StandInOp::Delete { id },
                None => return Ok(()),
            },
            8 => match known {
                Some(id) => StandInOp::Stamp { id },
                None => return Ok(()),
            },
            _ => StandInOp::CrashIf {
                device: DEVICES[pick % DEVICES.len()].to_owned(),
            },
        };
        let mode = if wait {
            Mode::WaitForServer
        } else {
            Mode::ShowAtOnce
        };
        let device = &mut self.devices[at];
        if let Ok(step) = device
            .client
            .local(&mut device.doc, StandIn::op(&op), mode, device.now)
        {
            self.changes += 1;
            device.made += 1;
            self.handle(at, step)?;
        }
        Ok(())
    }

    /// The page is loaded again: a new client, started with the host's crash note.
    fn reload(&mut self, at: usize) {
        self.disconnect(at);
        let old = &mut self.devices[at];
        let device = Device::reloaded(
            old.name,
            old.doc.note,
            std::mem::take(&mut old.doc.crashed),
            std::mem::take(&mut old.doc.again),
            old.now,
        );
        self.devices[at] = device;
    }

    fn quiet(&self) -> bool {
        self.devices.iter().all(|device| {
            device.conn.is_some_and(|conn| self.sim.waiting(conn) == 0)
                && device.to_server.is_empty()
                && device.client.phase() == Phase::Live
                && device.client.pending() == 0
                && device.client.unsent() == 0
        })
    }

    /// Restores everyone's rights, reconnects everyone and delivers every frame until nothing
    /// moves.
    fn settle(&mut self) -> Result<(), TestCaseError> {
        for at in 0..self.devices.len() {
            let client = self.devices[at].client.client();
            self.sim.set_can_edit(client, true);
        }
        for _ in 0..2_000 {
            self.sim.give_verdicts();
            for at in 0..self.devices.len() {
                self.connect(at)?;
            }
            for _ in 0..1_000 {
                let mut moved = false;
                for at in 0..self.devices.len() {
                    if self.devices[at].loading {
                        moved = true;
                        self.base_ready(at)?;
                    }
                    let device = &self.devices[at];
                    let to_server = device.to_server.len();
                    let to_client = device.conn.map_or(0, |conn| self.sim.waiting(conn));
                    if to_server + to_client > 0 {
                        moved = true;
                    }
                    self.deliver_to_server(at, to_server);
                    self.deliver_to_client(at, to_client)?;
                }
                if !moved {
                    break;
                }
            }
            if self.quiet() {
                return Ok(());
            }
            for at in 0..self.devices.len() {
                self.tick(at, 1_000);
            }
        }
        let phases: Vec<_> = self
            .devices
            .iter()
            .map(|device| (device.client.phase(), device.client.pending()))
            .collect();
        Err(TestCaseError::fail(format!("never settled: {phases:?}")))
    }

    /// No change reported refused is in the log, and every change a client still running made
    /// is in the log, refused, or dropped for lost edit rights.
    fn check_refusals(&self) -> Result<(), TestCaseError> {
        let stored: HashSet<(ClientId, u64)> = self
            .sim
            .log()
            .iter()
            .filter_map(|entry| match &entry.body {
                EntryBody::Change { client, local, .. } => Some((*client, local.0)),
                EntryBody::Cancel { .. } => None,
            })
            .collect();
        for refused in &self.refused {
            prop_assert!(!stored.contains(refused), "{refused:?} refused but stored");
        }
        for device in &self.devices {
            let client = device.client.client();
            for local in 1..=device.made {
                prop_assert!(
                    stored.contains(&(client, local))
                        || device.refused.contains(&local)
                        || device.dropped.contains(&local),
                    "device {} change {local} was lost",
                    device.name
                );
            }
        }
        Ok(())
    }

    /// A fresh document with the server's log applied in order.
    fn replay(&self) -> StandIn {
        let mut fresh = StandIn::new("fresh");
        for entry in self.sim.log() {
            if let EntryBody::Change {
                participant,
                payload,
                cancelled: false,
                ..
            } = &entry.body
            {
                fresh
                    .apply(&EntryView {
                        seq: Some(entry.seq),
                        participant: *participant,
                        own: false,
                        stamp: Some(Stamp {
                            at_ms: entry.at_ms,
                            seed: entry.seed,
                        }),
                        payload,
                    })
                    .unwrap();
            }
        }
        fresh
    }
}

#[derive(Clone, Debug)]
enum Action {
    Change {
        device: usize,
        kind: u8,
        pick: usize,
        wait: bool,
    },
    ToServer {
        device: usize,
        count: usize,
    },
    ToClient {
        device: usize,
        count: usize,
    },
    Tick {
        device: usize,
        ms: u64,
    },
    Disconnect {
        device: usize,
    },
    Reconnect {
        device: usize,
    },
    Expire {
        device: usize,
    },
    Access {
        device: usize,
        can_edit: bool,
    },
    Reload {
        device: usize,
    },
    BaseReady {
        device: usize,
    },
    Verdicts,
}

fn action() -> impl Strategy<Value = Action> {
    let device = 0..DEVICES.len();
    prop_oneof![
        14 => (device.clone(), 0u8..10, any::<usize>(), prop::bool::weighted(0.2)).prop_map(
            |(device, kind, pick, wait)| Action::Change { device, kind, pick, wait }
        ),
        6 => (device.clone(), 1usize..4).prop_map(|(device, count)| Action::ToServer { device, count }),
        8 => (device.clone(), 1usize..6).prop_map(|(device, count)| Action::ToClient { device, count }),
        2 => (device.clone(), prop_oneof![1u64..1_500, 50_000u64..70_000])
            .prop_map(|(device, ms)| Action::Tick { device, ms }),
        1 => device.clone().prop_map(|device| Action::Disconnect { device }),
        2 => device.clone().prop_map(|device| Action::Reconnect { device }),
        1 => device.clone().prop_map(|device| Action::Expire { device }),
        1 => (device.clone(), any::<bool>()).prop_map(|(device, can_edit)| Action::Access { device, can_edit }),
        1 => device.clone().prop_map(|device| Action::Reload { device }),
        3 => device.prop_map(|device| Action::BaseReady { device }),
        2 => Just(Action::Verdicts),
    ]
}

fn run(actions: &[Action]) -> Result<(), TestCaseError> {
    run_with(actions, true)
}

/// Runs a session; `delay_verdicts` says whether verdicts wait for a `Verdicts` action.
fn run_with(actions: &[Action], delay_verdicts: bool) -> Result<(), TestCaseError> {
    let mut world = World::new();
    world.sim.delay_verdicts = delay_verdicts;
    // Everyone joins first, so the early changes have somewhere to go.
    world.settle()?;
    for action in actions {
        match *action {
            Action::Change {
                device,
                kind,
                pick,
                wait,
            } => world.change(device, kind, pick, wait)?,
            Action::ToServer { device, count } => world.deliver_to_server(device, count),
            Action::ToClient { device, count } => world.deliver_to_client(device, count)?,
            Action::Tick { device, ms } => world.tick(device, ms),
            Action::Disconnect { device } => world.disconnect(device),
            Action::Reconnect { device } => world.connect(device)?,
            Action::Expire { device } => {
                let client = world.devices[device].client.client();
                world.sim.expire(client);
            }
            Action::Access { device, can_edit } => {
                let client = world.devices[device].client.client();
                world.sim.set_can_edit(client, can_edit);
            }
            Action::Reload { device } => world.reload(device),
            Action::BaseReady { device } => world.base_ready(device)?,
            Action::Verdicts => world.sim.give_verdicts(),
        }
    }
    world.settle()?;

    let expected = world.replay();
    for device in &world.devices {
        prop_assert_eq!(device.client.pending(), 0);
        prop_assert_eq!(device.client.phase(), Phase::Live);
        prop_assert_eq!(
            device.client.agreed_seq(),
            Seq(world.sim.log().len() as u64)
        );
        prop_assert_eq!(device.doc.note, None, "device {}", device.name);
        prop_assert!(
            device.doc.again.is_empty(),
            "device {} applied {:?} again after crashing in it",
            device.name,
            device.doc.again
        );
        // Lines, ids and the id table alike, not only the texts.
        prop_assert_eq!(
            device.doc.doc.content(),
            expected.content(),
            "device {}",
            device.name
        );
    }
    world.check_refusals()?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_sessions_converge(
        actions in prop::collection::vec(action(), 0..400),
        delay_verdicts in any::<bool>(),
    ) {
        run_with(&actions, delay_verdicts)?;
    }
}

/// One session written out by hand that meets every path the random ones are meant to meet:
/// a crash during a direct apply, a crash during a go-back, the cancel and the resync, a
/// reload with the crash note, a dropped connection and an expired place.
#[test]
fn scripted_session_converges() {
    let mut world = World::new();
    world.settle().unwrap();
    // a and b each make a change.
    world.change(0, 0, 0, false).unwrap();
    world.change(1, 0, 1, false).unwrap();
    world.deliver_to_server(0, 10);
    world.deliver_to_server(1, 10);
    // b changes a line and waits for it, so it keeps a copy while the others arrive.
    world.change(1, 5, 0, false).unwrap();
    // a's next two changes stop c's editor (applying straight on) and b's (going back).
    world.change(0, 9, 2, false).unwrap(); // CrashIf { device: "c" }
    world.change(0, 9, 1, false).unwrap(); // CrashIf { device: "b" }
    world.deliver_to_server(0, 10);
    for at in 0..3 {
        world.deliver_to_client(at, 20).unwrap();
    }
    assert!(matches!(
        world.devices[1].client.phase(),
        Phase::Stalled { .. }
    ));
    assert!(matches!(
        world.devices[2].client.phase(),
        Phase::Stalled { .. }
    ));
    assert_eq!(world.devices[1].client.pending(), 1);
    // c drops before its report reaches the server, then its page is loaded again.
    world.disconnect(2);
    world.reload(2);
    // b's place expires with its change and its report in flight.
    world.change(1, 0, 3, true).unwrap();
    let b = world.devices[1].client.client();
    world.sim.expire(b);
    world.deliver_to_server(1, 10);
    world.settle().unwrap();
    let expected = world.replay();
    assert!(!expected.lines().is_empty());
    world.check_refusals().unwrap();
    for device in &world.devices {
        assert_eq!(
            device.doc.doc.content(),
            expected.content(),
            "device {}",
            device.name
        );
        assert_eq!(device.doc.note, None);
        assert_eq!(device.client.pending(), 0);
    }
    let cancels = world
        .sim
        .log()
        .iter()
        .filter(|entry| matches!(entry.body, EntryBody::Cancel { .. }))
        .count();
    assert_eq!(cancels, 2);
    assert!(
        world.sim.resyncs > 0,
        "a's editor applied both and loaded the base again"
    );
}

/// Sessions the random runs found when the review's three faults were put back, kept so each
/// is always tried.
mod found {
    use super::{Action::*, run, run_with};

    /// A own waiting change that crashes the editor, confirmed during a catch-up from the base
    /// before any copy exists (review I1).
    #[test]
    fn own_crash_confirmed_during_a_base_catch_up() {
        run_with(
            &[
                Change {
                    device: 2,
                    kind: 9,
                    pick: 1254765286747124472,
                    wait: false,
                },
                ToServer {
                    device: 2,
                    count: 1,
                },
                Change {
                    device: 2,
                    kind: 9,
                    pick: 1819974019953870530,
                    wait: true,
                },
                ToClient {
                    device: 0,
                    count: 1,
                },
            ],
            false,
        )
        .unwrap();
    }

    /// A verdict that reaches a reloaded client while it loads the base (review I2).
    #[test]
    fn verdict_held_while_a_reloaded_client_loads_the_base() {
        let mut actions = vec![
            Change {
                device: 0,
                kind: 0,
                pick: 0,
                wait: false
            };
            15
        ];
        actions.extend([
            Access {
                device: 2,
                can_edit: false,
            },
            Change {
                device: 1,
                kind: 9,
                pick: 5668238091124939601,
                wait: false,
            },
            ToServer {
                device: 1,
                count: 1,
            },
            Reconnect { device: 0 },
            Change {
                device: 0,
                kind: 0,
                pick: 0,
                wait: false,
            },
            Change {
                device: 0,
                kind: 0,
                pick: 0,
                wait: false,
            },
            ToClient {
                device: 2,
                count: 2,
            },
            ToServer {
                device: 2,
                count: 1,
            },
            Reload { device: 2 },
            Reconnect { device: 2 },
            ToServer {
                device: 2,
                count: 1,
            },
        ]);
        run(&actions).unwrap();
    }

    /// A reload while waiting for a verdict after an expiry and a base load (review I3).
    #[test]
    fn reload_while_waiting_for_a_verdict() {
        run(&[
            Change {
                device: 1,
                kind: 0,
                pick: 0,
                wait: false,
            },
            Change {
                device: 1,
                kind: 0,
                pick: 0,
                wait: false,
            },
            Change {
                device: 1,
                kind: 0,
                pick: 0,
                wait: false,
            },
            Access {
                device: 1,
                can_edit: false,
            },
            Change {
                device: 0,
                kind: 0,
                pick: 0,
                wait: false,
            },
            Change {
                device: 0,
                kind: 0,
                pick: 0,
                wait: false,
            },
            Change {
                device: 0,
                kind: 9,
                pick: 3671932346002311409,
                wait: false,
            },
            ToServer {
                device: 0,
                count: 3,
            },
            Expire { device: 1 },
            ToClient {
                device: 1,
                count: 5,
            },
            Reconnect { device: 1 },
            ToServer {
                device: 1,
                count: 1,
            },
            ToClient {
                device: 1,
                count: 3,
            },
            BaseReady { device: 1 },
            Reload { device: 1 },
        ])
        .unwrap();
    }
}

/// The run and block limits depend only on the agreed table, so every device decides the same
/// way: one loaded from the stored form, one that made the same changes, one that reloaded.
#[test]
fn fragmentation_limit_is_decided_alike_everywhere() {
    use collab_live::ids::{IdError, LiveId, OrderedIds};
    let runs = OrderedIds::MAX_RUNS as u32 - 4;
    let live: Vec<_> = (0..runs)
        .map(|at| serde_json::json!({ "participant": 1, "start": at * 2, "len": 1 }))
        .collect();
    let stored = serde_json::json!({ "live": live, "deleted": [] });
    let mut a: OrderedIds = serde_json::from_value(stored.clone()).unwrap();
    let mut b: OrderedIds = serde_json::from_value(stored).unwrap();
    // Both make one change that fits.
    a.move_block(10, 4, 50).unwrap();
    b.move_block(10, 4, 50).unwrap();
    // A third device loads the result from the stored form.
    let mut c: OrderedIds = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
    let plan = [(0u32, 1u32), (5, 1)];
    let mut outcomes = Vec::new();
    for ids in [&mut a, &mut b, &mut c] {
        let check = ids.check_fragmentation(&plan);
        let before = serde_json::to_string(&*ids).unwrap();
        let first = ids.move_block(0, 1, 9);
        let mut made = 0u32;
        let refusal = loop {
            match ids.insert(0, LiveId(7, made), 1) {
                Ok(()) => made += 1,
                Err(error) => break error,
            }
        };
        let after = serde_json::to_string(&*ids).unwrap();
        outcomes.push((check, first, made, refusal, before != after, after));
    }
    assert!(
        outcomes.windows(2).all(|pair| pair[0] == pair[1]),
        "{:?}",
        {
            outcomes
                .iter()
                .map(|(check, first, made, refusal, ..)| (*check, *first, *made, *refusal))
                .collect::<Vec<_>>()
        }
    );
    // The check said no (two blocks could pass the limit), the single move fitted, and the
    // inserts after it stopped at the same place everywhere.
    let (check, first, made, refusal, changed, _) = &outcomes[0];
    assert!(matches!(check, Err(IdError::TooFragmented { .. })));
    assert_eq!(*first, Ok(()));
    assert_eq!(*made, 3);
    assert!(matches!(refusal, IdError::TooFragmented { .. }));
    assert!(*changed);
}
