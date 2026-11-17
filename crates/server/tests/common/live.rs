//! A real `collab-live` client driven over a real WebSocket.

use std::time::Instant;

use collab_core::ids::{DocId, Seq};
use collab_live::client::{Event, LiveClient, LiveConfig, LocalError, Mode, Phase, Step};
use collab_live::ids::LiveId;
use collab_live::stand_in::{StandIn, StandInOp};
use futures_util::{SinkExt, StreamExt};
use protocol::live::{LiveClientMsg, LiveServerMsg, Place};
use tokio_tungstenite::tungstenite::Message;

use super::{Site, Socket, TIMEOUT, TestUser, open_socket};

pub const FORMAT: &str = "gridcraft/stand-in-1";

pub struct LiveTestClient {
    doc: DocId,
    user: TestUser,
    device: String,
    started: Instant,
    socket: Option<Socket>,
    client: LiveClient<StandIn>,
    editor: StandIn,
    drop_next_entry_frame: bool,
    drop_next_server_entry_frame: bool,
    dropped_server_entry_frames: usize,
    pub events: Vec<Event>,
    pub outbound_messages: Vec<LiveClientMsg>,
    pub messages: Vec<LiveServerMsg>,
    pub raw_messages: Vec<String>,
}

impl LiveTestClient {
    pub async fn connect(site: &Site, doc: DocId, user: &TestUser, device: &str) -> LiveTestClient {
        let mut this = LiveTestClient {
            doc,
            user: user.clone(),
            device: device.to_owned(),
            started: Instant::now(),
            socket: None,
            client: LiveClient::new(LiveConfig {
                format: FORMAT.to_owned(),
                crashed_at: None,
            }),
            editor: StandIn::new(device),
            drop_next_entry_frame: false,
            drop_next_server_entry_frame: false,
            dropped_server_entry_frames: 0,
            events: Vec::new(),
            outbound_messages: Vec::new(),
            messages: Vec::new(),
            raw_messages: Vec::new(),
        };
        this.reconnect(site).await;
        this.pump_until(|client| client.client.phase() == Phase::Live)
            .await;
        this
    }

    pub async fn reconnect(&mut self, site: &Site) {
        self.client.disconnected(self.now_ms());
        self.socket = Some(open_socket(site.addr, self.doc, &self.user).await);
        let frames = self.client.connected(self.now_ms());
        self.send_frames(frames).await;
    }

    pub fn disconnect(&mut self) {
        self.socket.take();
        self.client.disconnected(self.now_ms());
    }

    pub async fn close(&mut self) {
        if let Some(frame) = self.client.leave() {
            self.send_raw(&frame).await;
        }
        if let Some(mut socket) = self.socket.take() {
            let _ = socket.close(None).await;
        }
    }

    pub async fn insert(&mut self, text: &str) -> LiveId {
        self.insert_before(None, text).await
    }

    pub async fn insert_before(&mut self, before: Option<LiveId>, text: &str) -> LiveId {
        let id = self.client.new_ids(1).expect("joined client has ids");
        let op = StandInOp::Insert {
            before,
            id,
            text: text.to_owned(),
        };
        self.change(op, Mode::ShowAtOnce).await.unwrap();
        id
    }

    pub async fn change(&mut self, op: StandInOp, mode: Mode) -> Result<(), LocalError> {
        self.payload(StandIn::op(&op), mode).await
    }

    pub async fn payload(&mut self, payload: String, mode: Mode) -> Result<(), LocalError> {
        let now_ms = self.now_ms();
        let step = self.client.local(&mut self.editor, payload, mode, now_ms)?;
        self.take_step(step).await;
        Ok(())
    }

    pub async fn send_raw(&mut self, frame: &str) {
        let socket = self.socket.as_mut().expect("client is connected");
        socket.send(Message::text(frame)).await.unwrap();
    }

    pub fn drop_next_entry_frame(&mut self) {
        self.drop_next_entry_frame = true;
    }

    pub fn drop_next_server_entry_frame(&mut self) {
        self.drop_next_server_entry_frame = true;
    }

    pub fn dropped_server_entry_frames(&self) -> usize {
        self.dropped_server_entry_frames
    }

    pub async fn resend_entry(&mut self, message: &LiveClientMsg) {
        assert!(matches!(message, LiveClientMsg::Entry { .. }));
        self.outbound_messages.push(message.clone());
        self.send_raw(&serde_json::to_string(message).unwrap())
            .await;
    }

    pub async fn pump_until(&mut self, done: impl Fn(&LiveTestClient) -> bool) {
        tokio::time::timeout(TIMEOUT, async {
            while !done(self) {
                self.pump_once(true).await;
            }
        })
        .await
        .expect("live client condition timed out");
    }

    pub async fn receive_without_tick(&mut self) {
        self.pump_once(false).await;
    }

    pub async fn pump_without_tick_until(&mut self, done: impl Fn(&LiveTestClient) -> bool) {
        tokio::time::timeout(TIMEOUT, async {
            while !done(self) {
                self.pump_once(false).await;
            }
        })
        .await
        .expect("live client condition timed out without ticking");
    }

    pub async fn receive_raw_only(&mut self) {
        let next = tokio::time::timeout(
            TIMEOUT,
            self.socket.as_mut().expect("client is connected").next(),
        )
        .await
        .expect("server frame timed out");
        if let Some(Ok(Message::Text(text))) = next {
            self.raw_messages.push(text.to_string());
            self.messages
                .push(serde_json::from_str(&text).expect("live server frame"));
        }
    }

    pub async fn tick_now(&mut self) {
        let frames = self.client.tick(self.now_ms());
        self.send_frames(frames).await;
    }

    pub async fn wait_closed(&mut self) {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let next = self
                    .socket
                    .as_mut()
                    .expect("client is connected")
                    .next()
                    .await;
                match next {
                    Some(Ok(Message::Text(text))) => self.take_server_frame(&text).await,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Binary(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                }
            }
        })
        .await
        .expect("socket did not close");
    }

    pub fn lines(&self) -> Vec<String> {
        self.editor.lines()
    }

    pub fn client(&self) -> &LiveClient<StandIn> {
        &self.client
    }

    pub fn last_place(&self) -> Option<Place> {
        self.messages
            .iter()
            .rev()
            .find_map(|message| match message {
                LiveServerMsg::LiveJoined { place, .. } => Some(*place),
                _ => None,
            })
    }

    pub fn has_event(&self, wanted: impl Fn(&Event) -> bool) -> bool {
        self.events.iter().any(wanted)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn pump_once(&mut self, tick: bool) {
        if tick {
            let frames = self.client.tick(self.now_ms());
            self.send_frames(frames).await;
        }
        let next = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            self.socket.as_mut().expect("client is connected").next(),
        )
        .await;
        match next {
            Ok(Some(Ok(Message::Text(text)))) => self.take_server_frame(&text).await,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) | Err(_) => {}
            Ok(Some(Ok(Message::Binary(_) | Message::Frame(_)))) => {}
            Ok(Some(Ok(Message::Close(_)) | Err(_))) | Ok(None) => {
                self.client.disconnected(self.now_ms());
                self.socket.take();
            }
        }
    }

    async fn take_server_frame(&mut self, frame: &str) {
        let message: LiveServerMsg = serde_json::from_str(frame).expect("live server frame");
        if self.drop_next_server_entry_frame && matches!(&message, LiveServerMsg::Entry { .. }) {
            self.drop_next_server_entry_frame = false;
            self.dropped_server_entry_frames += 1;
            return;
        }
        self.raw_messages.push(frame.to_owned());
        self.messages.push(message);
        let now_ms = self.now_ms();
        let step = self
            .client
            .receive(&mut self.editor, frame, now_ms)
            .expect("live client accepted server frame");
        self.take_step(step).await;
    }

    async fn take_step(&mut self, mut step: Step) {
        loop {
            self.send_frames(std::mem::take(&mut step.send)).await;
            let load = step
                .events
                .iter()
                .any(|event| matches!(event, Event::LoadBase { at: Seq(0), .. }));
            self.events.append(&mut step.events);
            if !load {
                break;
            }
            self.editor = StandIn::new(&self.device);
            let now_ms = self.now_ms();
            step = self
                .client
                .base_ready(&mut self.editor, now_ms)
                .expect("empty live base is ready");
        }
    }

    async fn send_frames(&mut self, frames: Vec<String>) {
        for frame in frames {
            let message = serde_json::from_str(&frame).expect("live client frame");
            let drop =
                self.drop_next_entry_frame && matches!(&message, LiveClientMsg::Entry { .. });
            if drop {
                self.drop_next_entry_frame = false;
            }
            self.outbound_messages.push(message);
            if drop {
                continue;
            }
            self.send_raw(&frame).await;
        }
    }
}
