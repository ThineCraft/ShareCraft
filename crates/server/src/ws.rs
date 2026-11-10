//! The WebSocket endpoint: one connection per browser tab, relaying between the socket and the
//! document's room.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{ClientId, DocId, Seq};
use collab_text::TextOp;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use protocol::ClientMsg;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::AppState;
use crate::room::{Command, ConnId, OUTBOUND_CAPACITY, Outbound};

/// Largest frame or message accepted from a client.
const MAX_MESSAGE: usize = 1024 * 1024;
/// How long a new connection has to send `Join`.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a join is retried when the room it found was just ending.
const JOIN_ATTEMPTS: usize = 3;
/// Longest display name, in characters.
const MAX_NAME: usize = 60;
/// Name used when none is given.
const DEFAULT_NAME: &str = "Guest";

/// Query parameters of `/ws/docs/{doc_id}`.
#[derive(Debug, Deserialize)]
pub(crate) struct JoinParams {
    name: Option<String>,
}

/// `GET /ws/docs/{doc_id}?name=...`: upgrades to a WebSocket connected to that document.
pub(crate) async fn upgrade(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Query(params): Query<JoinParams>,
    ws: WebSocketUpgrade,
) -> Response {
    let Ok(id) = Uuid::parse_str(&doc_id) else {
        return (StatusCode::BAD_REQUEST, "invalid document id").into_response();
    };
    let name = display_name(params.name.as_deref());
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection(state, DocId(id), name, socket))
}

/// Trims `raw`, keeps at most 60 characters and falls back to "Guest".
fn display_name(raw: Option<&str>) -> String {
    let kept: String = raw.unwrap_or("").trim().chars().take(MAX_NAME).collect();
    let kept = kept.trim_end();
    if kept.is_empty() {
        DEFAULT_NAME.to_owned()
    } else {
        kept.to_owned()
    }
}

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

async fn connection(state: AppState, doc: DocId, name: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    if let Some((client, since)) = first_join(&mut stream).await {
        let conn = state.next_conn();
        if let Some((room, out_rx)) = join_room(&state, doc, conn, client, name, since).await {
            relay(&mut sink, &mut stream, &room, out_rx, conn, client).await;
            // The room ignores this if it already dropped or replaced the connection.
            let _ = room.send(Command::Leave { conn, client }).await;
        }
    }
    // The client may already be gone; there is nothing more to do either way.
    let _ = sink.send(Message::Close(None)).await;
}

/// Waits for the first data message, which must be `Join`. `None` closes the connection.
async fn first_join(stream: &mut Stream) -> Option<(ClientId, Option<Seq>)> {
    let deadline = tokio::time::Instant::now() + JOIN_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()?;
        match frame? {
            Ok(Message::Text(text)) => {
                return match serde_json::from_str::<ClientMsg<TextOp>>(text.as_str()) {
                    Ok(ClientMsg::Join { client, since }) => Some((client, since)),
                    _ => None,
                };
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Binary(_) | Message::Close(_)) | Err(_) => return None,
        }
    }
}

/// Joins the document's room, opening a fresh room if the one found was just ending.
async fn join_room(
    state: &AppState,
    doc: DocId,
    conn: ConnId,
    client: ClientId,
    name: String,
    since: Option<Seq>,
) -> Option<(mpsc::Sender<Command>, mpsc::Receiver<Outbound>)> {
    for _ in 0..JOIN_ATTEMPTS {
        let room = state.open_room(doc);
        let (tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, was_accepted) = oneshot::channel();
        let join = Command::Join {
            conn,
            client,
            name: name.clone(),
            since,
            tx,
            accepted,
        };
        if room.send(join).await.is_ok() && was_accepted.await.is_ok() {
            return Some((room, out_rx));
        }
    }
    None
}

/// Moves frames between the socket and the room until either side ends the connection.
async fn relay(
    sink: &mut Sink,
    stream: &mut Stream,
    room: &mpsc::Sender<Command>,
    mut out_rx: mpsc::Receiver<Outbound>,
    conn: ConnId,
    client: ClientId,
) {
    loop {
        tokio::select! {
            out = out_rx.recv() => {
                // `None`: the room dropped or replaced this connection.
                let Some(out) = out else { return };
                if write(sink, out).await.is_err() {
                    return;
                }
            }
            frame = stream.next() => {
                let cmd = match frame {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMsg<TextOp>>(text.as_str()) {
                            Ok(ClientMsg::Submit { op_id, base, op }) => Command::Submit {
                                conn,
                                client,
                                op_id,
                                base,
                                op,
                            },
                            Ok(ClientMsg::Cursor { at }) => Command::Cursor { conn, client, at },
                            // A second `Join` on one socket, or anything unparseable.
                            Ok(ClientMsg::Join { .. }) | Err(_) => return,
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    // Binary frames, close, oversized frames and transport errors.
                    Some(Ok(Message::Binary(_) | Message::Close(_)) | Err(_)) | None => return,
                };
                if room.send(cmd).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn write(sink: &mut Sink, out: Outbound) -> Result<(), axum::Error> {
    match out {
        Outbound::One(frame) => sink.send(Message::Text(frame)).await,
        Outbound::Many(frames) => {
            for frame in frames {
                sink.feed(Message::Text(frame)).await?;
            }
            sink.flush().await
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_limited_and_defaulted() {
        assert_eq!(display_name(None), "Guest");
        assert_eq!(display_name(Some("   ")), "Guest");
        assert_eq!(display_name(Some("  Ann  ")), "Ann");
        let long = "é".repeat(80);
        assert_eq!(display_name(Some(&long)).chars().count(), 60);
    }
}
