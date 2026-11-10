//! The WebSocket endpoint: one connection per browser tab, relaying between the socket and the
//! document's room.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{ClientId, DocId, SessionId, UserId};
use collab_text::TextOp;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, Resume};
use records::Role;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::{Signed, same_origin};
use crate::room::{Command, ConnId, JoinReply, OUTBOUND_CAPACITY, Outbound};

/// Largest frame or message accepted from a client.
const MAX_MESSAGE: usize = 1024 * 1024;
/// How long a new connection has to send `Join`.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a closing connection waits for the client to answer its close.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How often a join is retried when the room it found was just ending.
const JOIN_ATTEMPTS: usize = 3;

/// Who a connection belongs to and what they may do, decided before the upgrade.
pub(crate) struct Visitor {
    pub(crate) user: UserId,
    pub(crate) session: SessionId,
    /// When the sign-in session ends; the connection closes then.
    pub(crate) expires_at: OffsetDateTime,
    /// The account's name, shown to the others and credited in checkpoints.
    pub(crate) name: String,
    pub(crate) role: Role,
    /// The document's name, which a room opened for this connection stores checkpoints under.
    pub(crate) doc_name: String,
}

/// `GET /ws/docs/{doc_id}`: upgrades to a WebSocket connected to that document. The caller is
/// the signed-in person (`401` without a session) and needs a role on the document; a document
/// that does not exist and one they have no role on are both `404`. Any query string, such as
/// the `name` older pages send, is ignored.
pub(crate) async fn upgrade(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    headers: HeaderMap,
    signed: Result<Signed, ApiError>,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "only local, same-origin connections are allowed",
        )
            .into_response();
    }
    let Ok(id) = Uuid::parse_str(&doc_id) else {
        return (StatusCode::BAD_REQUEST, "invalid document id").into_response();
    };
    let doc = DocId(id);
    let visitor = match visitor(&state, signed, doc).await {
        Ok(visitor) => visitor,
        Err(e) => return e.into_response(),
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection(state, doc, visitor, socket))
}

/// The signed-in caller with their role on `doc` and the document's name.
async fn visitor(
    state: &AppState,
    signed: Result<Signed, ApiError>,
    doc: DocId,
) -> Result<Visitor, ApiError> {
    let session = signed?.session;
    let db = state.db();
    let role = db
        .role(session.user.id, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let document = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    Ok(Visitor {
        user: session.user.id,
        session: session.id,
        expires_at: session.expires_at,
        name: session.user.name,
        role,
        doc_name: document.name,
    })
}

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

async fn connection(state: AppState, doc: DocId, visitor: Visitor, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    if let Some((client, resume)) = first_join(&mut stream).await {
        let conn = state.next_conn();
        if let Some((room, out_rx)) = join_room(&state, doc, conn, client, &visitor, resume).await {
            if recheck_after_join(&state, doc, &room, &visitor).await {
                let ends = Ending {
                    session: visitor.session,
                    at: deadline(visitor.expires_at, OffsetDateTime::now_utc()),
                };
                relay(&mut sink, &mut stream, &room, out_rx, conn, client, ends).await;
            }
            // The room ignores this if it already dropped or replaced the connection.
            let _ = room.send(Command::Leave { conn, client }).await;
        }
    }
    // The client may already be gone; there is nothing more to do either way.
    let _ = sink.send(Message::Close(None)).await;
    // Read until the client answers the close. Frames it sent meanwhile are then consumed: left
    // unread, they make the transport reset the connection, which can discard the frames just
    // written to the client, such as `Removed`.
    let _ = tokio::time::timeout(CLOSE_WAIT, async {
        while let Some(Ok(frame)) = stream.next().await {
            if matches!(frame, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
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
    visitor: &Visitor,
    resume: Option<Resume>,
) -> Option<(mpsc::Sender<Command>, mpsc::Receiver<Outbound>)> {
    for _ in 0..JOIN_ATTEMPTS {
        let room = state.open_room(doc, visitor.doc_name.clone());
        let (tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, was_accepted) = oneshot::channel();
        let join = Command::Join {
            conn,
            client,
            user: visitor.user,
            session: visitor.session,
            name: visitor.name.clone(),
            role: visitor.role,
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

/// Closes the gap between the access check before the upgrade and the room accepting the
/// join: access may have ended in between, and the hooks that tell rooms about it may have run
/// before this connection was a member. Reads the session and the role again; a session that
/// has ended removes its connections (`Removed { signed_out }`), and a role that differs is
/// handed to the room as an access change, which removes or adjusts every connection of that
/// person.
///
/// Returns `false` when the records could not be read; the connection then leaves quietly,
/// and the page reconnects and is checked again.
pub(crate) async fn recheck_after_join(
    state: &AppState,
    doc: DocId,
    room: &mpsc::Sender<Command>,
    visitor: &Visitor,
) -> bool {
    let db = state.db();
    let cmd = match db.session_by_id(visitor.session).await {
        Ok(None) => Some(Command::EndSession {
            session: visitor.session,
        }),
        Ok(Some(_)) => match db.role(visitor.user, doc).await {
            Ok(role) if role == Some(visitor.role) => None,
            Ok(role) => Some(Command::SetAccess {
                user: visitor.user,
                role,
            }),
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                return false;
            }
        },
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return false;
        }
    };
    if let Some(cmd) = cmd {
        // A room that has ended has dropped this connection already.
        let _ = room.send(cmd).await;
    }
    true
}

/// When and how a connection's sign-in runs out.
struct Ending {
    session: SessionId,
    /// `None`: too far ahead to wait for.
    at: Option<Instant>,
}

/// The moment `expires_at` arrives, measured from `now`: at once if it has passed, `None` if it
/// lies further ahead than a timer can represent.
fn deadline(expires_at: OffsetDateTime, now: OffsetDateTime) -> Option<Instant> {
    let left = std::time::Duration::try_from(expires_at - now).unwrap_or(Duration::ZERO);
    Instant::now().checked_add(left)
}

/// Moves frames between the socket and the room until either side ends the connection. When
/// the session expires, asks the room to end the session's connections; the room then queues
/// `Removed { signed_out }` and lets go, which ends this loop once the frame is written.
async fn relay(
    sink: &mut Sink,
    stream: &mut Stream,
    room: &mpsc::Sender<Command>,
    mut out_rx: mpsc::Receiver<Outbound>,
    conn: ConnId,
    client: ClientId,
    mut ends: Ending,
) {
    loop {
        tokio::select! {
            out = out_rx.recv() => {
                // `None`: the room dropped, replaced or removed this connection, after every
                // frame it queued (such as `Removed`) has been written.
                let Some(out) = out else { return };
                if write(sink, out).await.is_err() {
                    return;
                }
            }
            () = sleep_until(ends.at) => {
                ends.at = None;
                if room.send(Command::EndSession { session: ends.session }).await.is_err() {
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

/// Waits until `at`; never ready when there is nothing to wait for.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
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
    fn session_end_becomes_a_timer_deadline() {
        let now = OffsetDateTime::now_utc();
        let before = Instant::now();
        // Already over: at once.
        let past = deadline(now - time::Duration::hours(1), now).unwrap();
        assert!(past <= Instant::now());
        // Ahead: that far from now.
        let soon = deadline(now + time::Duration::seconds(90), now).unwrap();
        assert!(soon >= before + Duration::from_secs(90));
        assert!(soon <= Instant::now() + Duration::from_secs(90));
        // As far ahead as a date goes: no overflow, whatever the platform's clock can hold.
        let far = OffsetDateTime::new_utc(
            time::Date::from_calendar_date(9999, time::Month::December, 31).unwrap(),
            time::Time::MIDNIGHT,
        );
        if let Some(at) = deadline(far, now) {
            assert!(at > soon);
        }
    }
}
