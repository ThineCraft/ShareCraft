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
use protocol::{ClientMsg, FileClientMsg, FileServerMsg, RemovedReason, Resume, ServerMsg};
use records::Role;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::{Signed, same_origin};
use crate::file_room::{FileCommand, FileLink};
use crate::room::{
    AccessChange, Command, ConnId, JoinReply, OUTBOUND_CAPACITY, Outbound, RoomLink,
};

/// Largest frame or message accepted from a client.
const MAX_MESSAGE: usize = 1024 * 1024;
/// How long a new connection has to send `Join`.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a closing connection waits for the client to answer its close.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How often a join is retried when the room it found was just ending.
const JOIN_ATTEMPTS: usize = 3;
// How long one write to the socket may take is `Config::write_timeout`.

/// Why the relay ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// The client left, the room let go of the connection or a socket error ended it.
    Normally,
    /// A write did not finish in time: the client has stopped reading, so nothing more can be
    /// sent to it, not even a close.
    WriteTimedOut,
}

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
    /// Whether the document is a file, opened in a file session rather than a text room.
    pub(crate) is_file: bool,
}

/// `GET /ws/docs/{doc_id}`: upgrades to a WebSocket connected to that document: its text room,
/// or its file session when the document is a file. The caller is the signed-in person (`401`
/// without a session) and needs a role on the document; a document that does not exist and one
/// they have no role on are both `404`. Any query string, such as the `name` older pages send,
/// is ignored.
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
        is_file: document.editor.is_file(),
        doc_name: document.name,
    })
}

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

async fn connection(state: AppState, doc: DocId, visitor: Visitor, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let limit = state.config().write_timeout;
    // The queue of a connection removed right after its join: never read, so that nothing the
    // room queued for it (its snapshot or seat token above all) reaches the client. Kept until
    // the connection ends, so the room sees it open while it removes the member.
    let mut unread = None;
    let mut session = Session {
        state: &state,
        doc,
        visitor: &visitor,
        sink: &mut sink,
        stream: &mut stream,
        unread: &mut unread,
        limit,
    };
    // The first data message must be the right kind of `Join`; anything else closes.
    let stalled = match first_text(session.stream).await {
        Some(first) if visitor.is_file => file_session(&mut session, &first).await,
        Some(first) => text_session(&mut session, &first).await,
        None => false,
    };
    if stalled {
        // The client is not reading, so a close would wait as long as the frame that timed
        // out, and the client is not going to answer it. The socket closes as it is dropped.
        return;
    }
    // The client may already be gone; there is nothing more to do either way.
    let _ = tokio::time::timeout(limit, sink.send(Message::Close(None))).await;
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

/// One connection, from its first message until it is closed.
struct Session<'a> {
    state: &'a AppState,
    doc: DocId,
    visitor: &'a Visitor,
    sink: &'a mut Sink,
    stream: &'a mut Stream,
    /// Where a connection removed right after its join keeps its unread queue.
    unread: &'a mut Option<mpsc::Receiver<Outbound>>,
    /// How long one write to the socket may take.
    limit: Duration,
}

/// Waits for the first data message. `None` closes the connection.
async fn first_text(stream: &mut Stream) -> Option<String> {
    let deadline = tokio::time::Instant::now() + JOIN_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()?;
        match frame? {
            Ok(Message::Text(text)) => return Some(text.as_str().to_owned()),
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Binary(_) | Message::Close(_)) | Err(_) => return None,
        }
    }
}

/// A text client's `Join`, if `text` is one. A file client's `Join`, which names no `resume`,
/// is the wrong kind of client for a text document.
fn text_join(text: &str) -> Option<(ClientId, Option<Resume>)> {
    if serde_json::from_str::<FileClientMsg>(text).is_ok() {
        return None;
    }
    match serde_json::from_str::<ClientMsg<TextOp>>(text) {
        Ok(ClientMsg::Join { client, resume }) => Some((client, resume)),
        _ => None,
    }
}

/// A text document's connection: joins its room with `first`, which must be a text client's
/// `Join`, and relays until the connection ends. Returns whether a write timed out.
async fn text_session(s: &mut Session<'_>, first: &str) -> bool {
    let Some((client, resume)) = text_join(first) else {
        return false;
    };
    let conn = s.state.next_conn();
    let Some((room, out_rx)) = join_room(s.state, s.doc, conn, client, s.visitor, resume).await
    else {
        return false;
    };
    let stalled = match recheck_after_join(s.state, s.doc, &room.access, s.visitor).await {
        Recheck::Keep => {
            let read = |text: &str| match serde_json::from_str::<ClientMsg<TextOp>>(text) {
                Ok(ClientMsg::Submit { op_id, base, op }) => Some(Command::Submit {
                    conn,
                    client,
                    op_id,
                    base,
                    op,
                }),
                Ok(ClientMsg::Cursor { at }) => Some(Command::Cursor { conn, client, at }),
                // A second `Join` on one socket, or anything unparseable.
                Ok(ClientMsg::Join { .. }) | Err(_) => None,
            };
            let ended = relay(s, &room.commands, &room.access, out_rx, read).await;
            ended == Ended::WriteTimedOut
        }
        Recheck::Removed(reason) => {
            *s.unread = Some(out_rx);
            write_removed(s, &ServerMsg::<TextOp>::Removed { reason }).await
        }
        // Nothing queued is written; the page reconnects and is checked again.
        Recheck::Unavailable => {
            drop(out_rx);
            false
        }
    };
    // The room ignores this if it already dropped or replaced the connection.
    let _ = room.commands.send(Command::Leave { conn, client }).await;
    stalled
}

/// A file document's connection: joins its file session with `first`, which must be a file
/// client's `Join`, and relays until the connection ends. A file client sends nothing after
/// its `Join`. Returns whether a write timed out.
async fn file_session(s: &mut Session<'_>, first: &str) -> bool {
    let Ok(FileClientMsg::Join { client }) = serde_json::from_str::<FileClientMsg>(first) else {
        return false;
    };
    let conn = s.state.next_conn();
    let Some((room, out_rx)) = join_file_room(s.state, s.doc, conn, client, s.visitor).await else {
        return false;
    };
    let stalled = match recheck_after_join(s.state, s.doc, &room.access, s.visitor).await {
        Recheck::Keep => {
            let ended = relay(s, &room.commands, &room.access, out_rx, |_| None).await;
            ended == Ended::WriteTimedOut
        }
        Recheck::Removed(reason) => {
            *s.unread = Some(out_rx);
            write_removed(s, &FileServerMsg::Removed { reason }).await
        }
        Recheck::Unavailable => {
            drop(out_rx);
            false
        }
    };
    let _ = room
        .commands
        .send(FileCommand::Leave { conn, client })
        .await;
    stalled
}

/// Writes the one `Removed` frame a connection removed right after its join gets. Returns
/// whether the write timed out.
async fn write_removed(s: &mut Session<'_>, removed: &impl serde::Serialize) -> bool {
    match serde_json::to_string(removed) {
        Ok(frame) => matches!(
            write(s.sink, Outbound::One(frame.into()), s.limit).await,
            Err(WriteError::TimedOut)
        ),
        Err(_) => false,
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
) -> Option<(RoomLink, mpsc::Receiver<Outbound>)> {
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
        if room.commands.send(join).await.is_err() {
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

/// Joins the document's file session, opening a fresh one if the one found was just ending.
async fn join_file_room(
    state: &AppState,
    doc: DocId,
    conn: ConnId,
    client: ClientId,
    visitor: &Visitor,
) -> Option<(FileLink, mpsc::Receiver<Outbound>)> {
    for _ in 0..JOIN_ATTEMPTS {
        let room = state.open_file_room(doc, visitor.doc_name.clone());
        let (tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, was_accepted) = oneshot::channel();
        let join = FileCommand::Join {
            conn,
            client,
            user: visitor.user,
            session: visitor.session,
            name: visitor.name.clone(),
            role: visitor.role,
            tx,
            accepted,
        };
        if room.commands.send(join).await.is_err() {
            continue;
        }
        match was_accepted.await {
            Ok(JoinReply::Accepted) => return Some((room, out_rx)),
            Ok(JoinReply::Refused) => return None,
            // The session ended before handling the join.
            Err(_) => {}
        }
    }
    None
}

/// Closes the gap between the access check before the upgrade and the room accepting the
/// join: access may have ended in between, and the hooks that tell rooms about it may have run
/// before this connection was a member. Reads the session and the role again; a session that
/// has ended removes its connections (`Removed { signed_out }`, or `account_disabled` for every
/// connection of the account when that is why), and a role that differs is handed to the room
/// (through `access`, its access channel) as an access change, which removes or adjusts every
/// connection of that person. The role is read and sent under the document's access lock, like
/// the API's changes, so the room hears the readings in database order.
///
/// Says whether the connection stays, was removed (the room has been told), or could not be
/// checked because the records could not be read; it then leaves quietly, and the page
/// reconnects and is checked again.
async fn recheck_after_join(
    state: &AppState,
    doc: DocId,
    access: &mpsc::UnboundedSender<AccessChange>,
    visitor: &Visitor,
) -> Recheck {
    let _in_order = state.rooms().order_access(doc).await;
    let db = state.db();
    let change = match db.session_by_id(visitor.session).await {
        Ok(None) => match db.user(visitor.user).await {
            Ok(Some(user)) if user.disabled => Some(AccessChange::RemoveUser { user: user.id }),
            Ok(_) => Some(AccessChange::EndSession {
                session: visitor.session,
            }),
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                return Recheck::Unavailable;
            }
        },
        Ok(Some(_)) => match db.role(visitor.user, doc).await {
            Ok(role) if role == Some(visitor.role) => None,
            Ok(role) => Some(AccessChange::SetAccess {
                user: visitor.user,
                role,
            }),
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                return Recheck::Unavailable;
            }
        },
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return Recheck::Unavailable;
        }
    };
    let Some(change) = change else {
        return Recheck::Keep;
    };
    let outcome = match change {
        AccessChange::RemoveUser { .. } => Recheck::Removed(RemovedReason::AccountDisabled),
        AccessChange::EndSession { .. } => Recheck::Removed(RemovedReason::SignedOut),
        AccessChange::SetAccess { role: None, .. } => {
            Recheck::Removed(RemovedReason::AccessRevoked)
        }
        AccessChange::SetAccess { role: Some(_), .. } => Recheck::Keep,
    };
    // The room drops the member and records the removal. A room that has ended has dropped
    // this connection already.
    let _ = access.send(change);
    outcome
}

/// What [`recheck_after_join`] decided about a connection.
enum Recheck {
    /// Access is unchanged, or changed but not ended: the connection carries on.
    Keep,
    /// Access has ended: the connection gets this one `Removed` frame and nothing else.
    Removed(RemovedReason),
    /// The records could not be read.
    Unavailable,
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

/// Moves frames between the socket and the room until either side ends the connection. Each
/// text message is turned into a command for the room by `read`; one it turns into nothing
/// ends the connection. When the session expires, asks the room (through `access`) to end the
/// session's connections; the room then queues `Removed { signed_out }` and lets go, which ends
/// this loop once the frame is written. Each write has the session's limit to finish.
async fn relay<C>(
    s: &mut Session<'_>,
    commands: &mpsc::Sender<C>,
    access: &mpsc::UnboundedSender<AccessChange>,
    mut out_rx: mpsc::Receiver<Outbound>,
    read: impl Fn(&str) -> Option<C>,
) -> Ended {
    let mut ends = Ending {
        session: s.visitor.session,
        at: deadline(s.visitor.expires_at, OffsetDateTime::now_utc()),
    };
    loop {
        tokio::select! {
            out = out_rx.recv() => {
                // `None`: the room dropped, replaced or removed this connection, after every
                // frame it queued (such as `Removed`) has been written.
                let Some(out) = out else { return Ended::Normally };
                match write(s.sink, out, s.limit).await {
                    Ok(()) => {}
                    Err(WriteError::TimedOut) => return Ended::WriteTimedOut,
                    Err(WriteError::Socket) => return Ended::Normally,
                }
            }
            () = sleep_until(ends.at) => {
                ends.at = None;
                if access.send(AccessChange::EndSession { session: ends.session }).is_err() {
                    return Ended::Normally;
                }
            }
            frame = s.stream.next() => {
                let cmd = match frame {
                    Some(Ok(Message::Text(text))) => match read(text.as_str()) {
                        Some(cmd) => cmd,
                        None => return Ended::Normally,
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    // Binary frames, close, oversized frames and transport errors.
                    Some(Ok(Message::Binary(_) | Message::Close(_)) | Err(_)) | None => {
                        return Ended::Normally;
                    }
                };
                if commands.send(cmd).await.is_err() {
                    return Ended::Normally;
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

/// Why a frame could not be written.
enum WriteError {
    /// The socket did not take the frame within the time allowed.
    TimedOut,
    /// The socket failed.
    Socket,
}

/// Writes `out` to the socket. Each step (a frame, the final flush) has `limit` to
/// finish, so a client that stopped reading cannot hold this task, and with it a place in the
/// room, forever.
async fn write(sink: &mut Sink, out: Outbound, limit: Duration) -> Result<(), WriteError> {
    async fn step(
        limit: Duration,
        done: impl std::future::Future<Output = Result<(), axum::Error>>,
    ) -> Result<(), WriteError> {
        match tokio::time::timeout(limit, done).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(WriteError::Socket),
            Err(_) => Err(WriteError::TimedOut),
        }
    }
    match out {
        Outbound::One(frame) => step(limit, sink.send(Message::Text(frame))).await,
        Outbound::Many(frames) => {
            for frame in frames {
                step(limit, sink.feed(Message::Text(frame))).await?;
            }
            step(limit, sink.flush()).await
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
