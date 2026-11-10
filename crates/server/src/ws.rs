//! The WebSocket endpoint: one connection per browser tab, relaying between the socket and the
//! document's room.

use std::net::IpAddr;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{ClientId, DocId};
use collab_text::TextOp;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, Resume};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::AppState;
use crate::room::{Command, ConnId, DEFAULT_NAME, JoinReply, OUTBOUND_CAPACITY, Outbound};

/// Largest frame or message accepted from a client.
const MAX_MESSAGE: usize = 1024 * 1024;
/// How long a new connection has to send `Join`.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a join is retried when the room it found was just ending.
const JOIN_ATTEMPTS: usize = 3;
/// Longest display name, in characters.
const MAX_NAME: usize = 60;

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
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !is_local_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "only local, same-origin connections are allowed",
        )
            .into_response();
    }
    let Ok(id) = Uuid::parse_str(&doc_id) else {
        return (StatusCode::BAD_REQUEST, "invalid document id").into_response();
    };
    let name = display_name(params.name.as_deref());
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection(state, DocId(id), name, socket))
}

/// Whether the request names a loopback host and, if it comes from a page, that page was served
/// by this server. Without sign-in, this keeps other websites open in the user's browser (and
/// DNS-rebinding hosts) away from documents.
fn is_local_request(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    if !host_part(host).is_some_and(is_loopback_host) {
        return false;
    }
    match headers.get(ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .is_ok_and(|origin| origin == format!("http://{host}")),
    }
}

/// The host of a `Host` header value (`host`, `host:port`, `[v6]` or `[v6]:port`).
fn host_part(value: &str) -> Option<&str> {
    let (host, port) = match value.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':')?)),
            }
        }
        None => match value.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (value, None),
        },
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) || host.is_empty() {
        return None;
    }
    Some(host)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
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
    if let Some((client, resume)) = first_join(&mut stream).await {
        let conn = state.next_conn();
        if let Some((room, out_rx)) = join_room(&state, doc, conn, client, name, resume).await {
            relay(&mut sink, &mut stream, &room, out_rx, conn, client).await;
            // The room ignores this if it already dropped or replaced the connection.
            let _ = room.send(Command::Leave { conn, client }).await;
        }
    }
    // The client may already be gone; there is nothing more to do either way.
    let _ = sink.send(Message::Close(None)).await;
}

/// Waits for the first data message, which must be `Join`. `None` closes the connection.
async fn first_join(stream: &mut Stream) -> Option<(ClientId, Option<Resume>)> {
    let deadline = tokio::time::Instant::now() + JOIN_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()?;
        match frame? {
            Ok(Message::Text(text)) => {
                return match serde_json::from_str::<ClientMsg<TextOp>>(text.as_str()) {
                    Ok(ClientMsg::Join { client, resume }) => Some((client, resume)),
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
    resume: Option<Resume>,
) -> Option<(mpsc::Sender<Command>, mpsc::Receiver<Outbound>)> {
    for _ in 0..JOIN_ATTEMPTS {
        let room = state.open_room(doc);
        let (tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, was_accepted) = oneshot::channel();
        let join = Command::Join {
            conn,
            client,
            name: name.clone(),
            resume,
            tx,
            accepted,
        };
        if room.send(join).await.is_err() {
            continue;
        }
        match was_accepted.await {
            Ok(JoinReply::Accepted) => return Some((room, out_rx)),
            Ok(JoinReply::Refused) => return None,
            // The room ended before handling the join.
            Err(_) => {}
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
    fn host_header_forms_are_parsed() {
        assert_eq!(host_part("127.0.0.1:8787"), Some("127.0.0.1"));
        assert_eq!(host_part("localhost"), Some("localhost"));
        assert_eq!(host_part("[::1]:8787"), Some("::1"));
        assert_eq!(host_part("[::1]"), Some("::1"));
        assert_eq!(host_part("evil.example:8787"), Some("evil.example"));
        assert_eq!(host_part("localhost:99999"), None);
        assert_eq!(host_part("[::1]x"), None);
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("LocalHost"));
        assert!(!is_loopback_host("evil.example"));
        assert!(!is_loopback_host("0.0.0.0"));
    }

    #[test]
    fn names_are_trimmed_limited_and_defaulted() {
        assert_eq!(display_name(None), "Guest");
        assert_eq!(display_name(Some("   ")), "Guest");
        assert_eq!(display_name(Some("  Ann  ")), "Ann");
        let long = "é".repeat(80);
        assert_eq!(display_name(Some(&long)).chars().count(), 60);
    }
}
