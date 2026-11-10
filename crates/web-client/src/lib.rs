#![forbid(unsafe_code)]
//! The browser's half of a document session, compiled to WebAssembly.
//!
//! [`ClientInner`] holds all the logic as plain Rust so it can be tested natively; [`Client`]
//! wraps it for JavaScript. The page only moves frames between the socket and the client and
//! reports text box changes; positions cross the boundary as UTF-16 offsets, the unit the
//! browser uses, and are converted here to the Unicode scalar values the protocol counts in.

use collab_core::client_state::{ClientError as StateError, ClientState, Outgoing};
use collab_core::ids::{ClientId, OpId, Seq};
use collab_text::{Component, TextDoc, TextError, TextOp};
use protocol::{ClientMsg, Participant, Resume, RoomEpoch, SaveState, ServerMsg};
use serde::Serialize;
use uuid::Uuid;
use wasm_bindgen::prelude::*;

/// The character index at UTF-16 offset `utf16` of `text`. `None` inside a surrogate pair or
/// past the end.
#[must_use]
pub fn utf16_to_char(text: &str, utf16: usize) -> Option<usize> {
    let mut units = 0usize;
    for (index, c) in text.chars().enumerate() {
        if units == utf16 {
            return Some(index);
        }
        if units > utf16 {
            return None;
        }
        units = units.saturating_add(c.len_utf16());
    }
    (units == utf16).then(|| text.chars().count())
}

/// The UTF-16 offset of character index `chars` of `text`. `None` past the end.
#[must_use]
pub fn char_to_utf16(text: &str, chars: usize) -> Option<usize> {
    let mut units = 0usize;
    let mut seen = 0usize;
    for c in text.chars() {
        if seen == chars {
            return Some(units);
        }
        units = units.saturating_add(c.len_utf16());
        seen = seen.saturating_add(1);
    }
    (seen == chars).then_some(units)
}

/// One step of a remote change, in UTF-16 units. Steps apply in order: each `start_utf16` is a
/// position in the text as the previous steps left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Edit {
    /// Where the step happens.
    pub start_utf16: usize,
    /// Units removed at `start_utf16`.
    pub removed_utf16: usize,
    /// Units inserted at `start_utf16`.
    pub inserted_utf16: usize,
}

/// Someone in the document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Person {
    /// Display name.
    pub name: String,
    /// Whether this is the person using this client.
    pub me: bool,
}

/// Whether this client's view of the document has reached Git.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SaveView {
    /// `saved` once everything this client has seen or sent is stored, `saving` while some of
    /// it is not, `failed` after the server reported a failed checkpoint (it retries).
    pub state: SaveState,
    /// The latest stored revision known.
    pub revision: Option<String>,
}

/// What a frame from the server asks of the page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Update {
    /// Whether the text changed; if so the text box must show `text`.
    pub text_changed: bool,
    /// The whole text, including this client's unconfirmed changes.
    pub text: String,
    /// The remote change in UTF-16 steps, so the page can keep the selection in place.
    pub edits: Vec<Edit>,
    /// A message to send now.
    pub send: Option<String>,
    /// Who is here, when that changed.
    pub people: Option<Vec<Person>>,
    /// Save progress; present once joined.
    pub save: Option<SaveView>,
    /// Unconfirmed local changes thrown away because the server could not take them back.
    pub dropped: usize,
    /// Present when the server reset this client to its text, saying what that cost.
    pub reset: Option<ResetKind>,
    /// Why the server refused a change, if it did.
    pub refused: Option<String>,
}

/// What a reset from the server means for the person, compared with the text they had,
/// including their unconfirmed changes. Serialized as `restarted_lost_text`, `restarted` or
/// `resynced`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    /// The room is a new one (the server restarted, or the room ended) and its text differs
    /// from what the person had: text they saw is gone.
    RestartedLostText,
    /// The room is a new one, with exactly the text the person had.
    Restarted,
    /// The same room, which no longer had the history to resume this client.
    Resynced,
}

/// Largest `Submit` frame the client sends, in bytes; the server closes the connection on a
/// frame over 1 MiB.
pub const MAX_SUBMIT_BYTES: usize = 512 * 1024;
/// How the message of [`ClientError::TooLarge`] begins, so the page can recognise it.
pub const TOO_LARGE: &str = "change_too_large";

/// Why the client could not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The change would make a `Submit` larger than [`MAX_SUBMIT_BYTES`]; nothing changed.
    #[error("{TOO_LARGE}: the change takes {0} bytes to send, more than {MAX_SUBMIT_BYTES}")]
    TooLarge(usize),
    /// The frame is not a server message.
    #[error("unreadable message from the server: {0}")]
    BadFrame(#[from] serde_json::Error),
    /// A position is inside a character or past the end of the text.
    #[error("position {0} is not a character boundary in the text")]
    BadPosition(usize),
    /// A change could not be applied, or a committed entry did not follow on.
    #[error("{0}")]
    Rejected(#[from] StateError<TextError>),
    /// A change could not be built.
    #[error("{0}")]
    Text(#[from] TextError),
}

/// The session logic behind [`Client`].
#[derive(Clone, Debug)]
pub struct ClientInner {
    state: ClientState<TextDoc>,
    /// The room lifetime from the last `Joined` or `Reset`; `None` until then, or after a
    /// refusal, so the next join starts afresh.
    epoch: Option<RoomEpoch>,
    /// Whether the server has answered the join sent on the current connection. Changes are
    /// held until then: their base may belong to a room that no longer exists.
    joined: bool,
    /// Highest sequence known to be stored.
    saved_through: Seq,
    save_failed: bool,
    revision: Option<String>,
}

impl ClientInner {
    /// A client identified as `client`, not yet in a room.
    #[must_use]
    pub fn new(client: ClientId) -> Self {
        Self {
            state: ClientState::new(client, TextDoc::default(), Seq(0)),
            epoch: None,
            joined: false,
            saved_through: Seq(0),
            save_failed: false,
            revision: None,
        }
    }

    /// The `Join` to send on a new connection, resuming where the client left off once it has
    /// been in a room. Calling it means it is being sent: changes are held until the reply.
    ///
    /// # Errors
    /// Only if the message cannot be encoded.
    pub fn join_message(&mut self) -> Result<String, ClientError> {
        self.joined = false;
        let resume = self.epoch.map(|epoch| Resume {
            epoch,
            seq: self.state.seq(),
        });
        encode(&ClientMsg::<TextOp>::Join {
            client: self.state.client(),
            resume,
        })
    }

    /// The text box replaced `removed_utf16` units at `start_utf16` with `inserted`. Returns
    /// the `Submit` to send now, or `None` if the change waits for an earlier one or for the
    /// join reply.
    ///
    /// # Errors
    /// `BadPosition` if an offset splits a character or is past the end; `TooLarge` if the
    /// change would not fit in one `Submit` of at most [`MAX_SUBMIT_BYTES`]. Nothing changes.
    pub fn local_replace(
        &mut self,
        start_utf16: usize,
        removed_utf16: usize,
        inserted: &str,
    ) -> Result<Option<String>, ClientError> {
        let text = self.state.doc().as_str();
        let start =
            utf16_to_char(text, start_utf16).ok_or(ClientError::BadPosition(start_utf16))?;
        let end_utf16 = start_utf16.saturating_add(removed_utf16);
        let end = utf16_to_char(text, end_utf16).ok_or(ClientError::BadPosition(end_utf16))?;
        let op = TextOp::replace(
            self.state.doc().len_chars(),
            start,
            end.saturating_sub(start),
            inserted,
        )?;
        let op_id = OpId(Uuid::new_v4());
        let size = submit(&Outgoing {
            op_id,
            base: self.state.seq(),
            op: op.clone(),
        })?
        .len();
        if size > MAX_SUBMIT_BYTES {
            return Err(ClientError::TooLarge(size));
        }
        let out = self.state.local(op_id, op)?;
        match out {
            Some(out) if self.joined => Ok(Some(submit(&out)?)),
            _ => Ok(None),
        }
    }

    /// Handles one frame from the server. On an error nothing has changed; the page should
    /// close the connection and join again.
    ///
    /// # Errors
    /// `BadFrame` for anything that is not a server message, `Rejected` for a committed entry
    /// that does not follow on or cannot be applied.
    pub fn receive(&mut self, frame: &str) -> Result<Update, ClientError> {
        let msg: ServerMsg<TextOp> = serde_json::from_str(frame)?;
        let mut update = Update::default();
        match msg {
            ServerMsg::Joined {
                epoch,
                seq,
                snapshot,
                revision,
                participants,
            } => {
                match snapshot {
                    Some(text) => self.adopt(&mut update, text, seq),
                    // The room still has our place: resend the change that may not have
                    // arrived. The server drops it if it was already committed.
                    None => update.send = self.resend_message()?,
                }
                self.joined_room(epoch, seq, revision);
                update.people = Some(self.people(participants));
            }
            ServerMsg::Reset {
                epoch,
                seq,
                snapshot,
                revision,
            } => {
                let same_text = self.state.doc().as_str() == snapshot;
                update.reset = Some(match (self.epoch == Some(epoch), same_text) {
                    (true, _) => ResetKind::Resynced,
                    (false, true) => ResetKind::Restarted,
                    (false, false) => ResetKind::RestartedLostText,
                });
                self.adopt(&mut update, snapshot, seq);
                self.joined_room(epoch, seq, revision);
            }
            ServerMsg::Committed { entry } => {
                let before = self.state.doc().as_str().to_owned();
                let mut next = self.state.clone();
                let applied = next.committed(&entry)?;
                if let Some(remote) = &applied.remote {
                    update.edits = utf16_edits(&before, remote)?;
                    update.text_changed = true;
                }
                if let Some(out) = &applied.send_next {
                    update.send = Some(submit(out)?);
                }
                self.state = next;
            }
            ServerMsg::People { participants } => {
                update.people = Some(self.people(participants));
            }
            ServerMsg::Save {
                state,
                revision,
                through,
            } => match state {
                SaveState::Saved => {
                    self.saved_through = self.saved_through.max(through);
                    self.save_failed = false;
                    if revision.is_some() {
                        self.revision = revision;
                    }
                }
                SaveState::Saving => self.save_failed = false,
                SaveState::Failed => self.save_failed = true,
            },
            ServerMsg::Refused { op_id, reason } => {
                // The refused change stays unconfirmed and would block every later one; the
                // next join starts afresh and reports it as dropped.
                if self.state.in_flight().is_some_and(|f| f.op_id == op_id) {
                    self.epoch = None;
                }
                update.refused = Some(reason);
            }
        }
        update.text = self.text();
        if self.epoch.is_some() {
            update.save = Some(self.save_view());
        }
        Ok(update)
    }

    /// The `Submit` for the change in flight, to resend after reconnecting.
    ///
    /// # Errors
    /// Only if the message cannot be encoded.
    pub fn resend_message(&self) -> Result<Option<String>, ClientError> {
        self.state.in_flight().as_ref().map(submit).transpose()
    }

    /// The text, including unconfirmed local changes.
    #[must_use]
    pub fn text(&self) -> String {
        self.state.doc().as_str().to_owned()
    }

    /// How many local changes the server has not confirmed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.state.pending()
    }

    /// Whether the server has answered the join sent on the current connection.
    #[must_use]
    pub fn joined(&self) -> bool {
        self.joined
    }

    /// Replaces the document with the server's, dropping unconfirmed changes.
    fn adopt(&mut self, update: &mut Update, snapshot: String, seq: Seq) {
        let before = self.text();
        update.dropped = self.state.reset(TextDoc::new(snapshot), seq);
        let after = self.state.doc().as_str();
        if before != after {
            update.text_changed = true;
            update.edits = diff_edit(&before, after).into_iter().collect();
        }
    }

    fn joined_room(&mut self, epoch: RoomEpoch, seq: Seq, revision: Option<String>) {
        self.epoch = Some(epoch);
        self.joined = true;
        // Anything the room held when we arrived is either stored or checkpointed shortly,
        // with a `Save` message saying so.
        self.saved_through = seq;
        self.save_failed = false;
        self.revision = revision;
    }

    fn people(&self, participants: Vec<Participant>) -> Vec<Person> {
        let me = self.state.client();
        participants
            .into_iter()
            .map(|p| Person {
                me: p.client == me,
                name: p.name,
            })
            .collect()
    }

    fn save_view(&self) -> SaveView {
        let state = if self.save_failed {
            SaveState::Failed
        } else if self.state.pending() > 0 || self.state.seq() > self.saved_through {
            SaveState::Saving
        } else {
            SaveState::Saved
        };
        SaveView {
            state,
            revision: self.revision.clone(),
        }
    }
}

fn encode(msg: &ClientMsg<TextOp>) -> Result<String, ClientError> {
    Ok(serde_json::to_string(msg)?)
}

fn submit(out: &Outgoing<TextOp>) -> Result<String, ClientError> {
    encode(&ClientMsg::Submit {
        op_id: out.op_id,
        base: out.base,
        op: out.op.clone(),
    })
}

/// The UTF-16 steps of `op` applied to `before`.
fn utf16_edits(before: &str, op: &TextOp) -> Result<Vec<Edit>, ClientError> {
    let mut old = before.chars();
    let mut at = 0usize;
    let mut edits = Vec::new();
    for component in op.components() {
        match component {
            Component::Retain(n) => at = at.saturating_add(utf16_len(&mut old, *n)?),
            Component::Insert(s) => {
                let inserted = s.encode_utf16().count();
                edits.push(Edit {
                    start_utf16: at,
                    removed_utf16: 0,
                    inserted_utf16: inserted,
                });
                at = at.saturating_add(inserted);
            }
            Component::Delete(n) => edits.push(Edit {
                start_utf16: at,
                removed_utf16: utf16_len(&mut old, *n)?,
                inserted_utf16: 0,
            }),
        }
    }
    Ok(edits)
}

/// The UTF-16 length of the next `n` characters of `chars`.
fn utf16_len(chars: &mut std::str::Chars<'_>, n: usize) -> Result<usize, ClientError> {
    let mut units = 0usize;
    for _ in 0..n {
        let c = chars.next().ok_or(TextError::OutOfRange)?;
        units = units.saturating_add(c.len_utf16());
    }
    Ok(units)
}

/// The single replacement turning `before` into `after`, keeping their common start and end.
fn diff_edit(before: &str, after: &str) -> Option<Edit> {
    if before == after {
        return None;
    }
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf16())
        .sum();
    let before_len = before.encode_utf16().count();
    let after_len = after.encode_utf16().count();
    let max_suffix = before_len.min(after_len).saturating_sub(prefix);
    let suffix: usize = before
        .chars()
        .rev()
        .zip(after.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf16())
        .scan(0usize, |total, units| {
            *total = total.saturating_add(units);
            (*total <= max_suffix).then_some(units)
        })
        .sum();
    Some(Edit {
        start_utf16: prefix,
        removed_utf16: before_len.saturating_sub(prefix).saturating_sub(suffix),
        inserted_utf16: after_len.saturating_sub(prefix).saturating_sub(suffix),
    })
}

/// A document session for the page. Every method that returns a message expects the page to
/// send it on the socket.
#[wasm_bindgen]
pub struct Client {
    inner: ClientInner,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Client {
    /// A client with a fresh random identifier.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Client {
        Client {
            inner: ClientInner::new(ClientId(Uuid::new_v4())),
        }
    }

    /// The `Join` to send as soon as a connection opens.
    ///
    /// # Errors
    /// Only if the message cannot be encoded.
    pub fn join_message(&mut self) -> Result<String, JsError> {
        Ok(self.inner.join_message()?)
    }

    /// The text box changed. Offsets are UTF-16 units, as the browser reports them. Returns a
    /// `Submit` to send now, or `undefined`.
    ///
    /// # Errors
    /// If an offset splits a character or is past the end, or if the change is too large to
    /// send in one piece (the error message then starts with `change_too_large`); nothing
    /// changes.
    pub fn local_replace(
        &mut self,
        start_utf16: u32,
        removed_utf16: u32,
        inserted: &str,
    ) -> Result<Option<String>, JsError> {
        Ok(self
            .inner
            .local_replace(units(start_utf16), units(removed_utf16), inserted)?)
    }

    /// A frame arrived. Returns the [`Update`] as JSON.
    ///
    /// # Errors
    /// If the frame is unreadable or does not follow on; nothing changes and the page should
    /// reconnect.
    pub fn receive(&mut self, frame: &str) -> Result<String, JsError> {
        let update = self.inner.receive(frame)?;
        Ok(serde_json::to_string(&update)?)
    }

    /// The `Submit` for the change in flight, if any.
    ///
    /// # Errors
    /// Only if the message cannot be encoded.
    pub fn resend_message(&self) -> Result<Option<String>, JsError> {
        Ok(self.inner.resend_message()?)
    }

    /// The text, including unconfirmed local changes.
    #[must_use]
    pub fn text(&self) -> String {
        self.inner.text()
    }

    /// How many local changes the server has not confirmed.
    #[must_use]
    pub fn pending(&self) -> u32 {
        u32::try_from(self.inner.pending()).unwrap_or(u32::MAX)
    }
}

fn units(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
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
    use collab_core::document::{Document, Entry};
    use collab_core::ids::{ClientId, OpId, Seq};
    use collab_text::{TextDoc, TextOp};
    use protocol::{ClientMsg, Resume, RoomEpoch, SaveState, ServerMsg};
    use uuid::Uuid;

    fn me() -> ClientId {
        ClientId(Uuid::from_u128(1))
    }
    fn other() -> ClientId {
        ClientId(Uuid::from_u128(2))
    }
    fn epoch(n: u128) -> RoomEpoch {
        RoomEpoch(Uuid::from_u128(n))
    }
    fn frame(msg: &ServerMsg<TextOp>) -> String {
        serde_json::to_string(msg).unwrap()
    }
    fn joined(epoch: RoomEpoch, seq: u64, snapshot: Option<&str>) -> String {
        frame(&ServerMsg::Joined {
            epoch,
            seq: Seq(seq),
            snapshot: snapshot.map(str::to_owned),
            revision: None,
            participants: vec![],
        })
    }
    fn committed(seq: u64, client: ClientId, op_id: OpId, op: TextOp) -> String {
        frame(&ServerMsg::Committed {
            entry: Entry {
                seq: Seq(seq),
                client,
                op_id,
                op,
            },
        })
    }
    fn parse(msg: &str) -> ClientMsg<TextOp> {
        serde_json::from_str(msg).unwrap()
    }
    /// A client that has joined a room holding `text` at sequence 0.
    fn joined_client(text: &str) -> ClientInner {
        let mut c = ClientInner::new(me());
        c.join_message().unwrap();
        c.receive(&joined(epoch(7), 0, Some(text))).unwrap();
        c
    }
    fn submitted(msg: &str) -> (OpId, Seq, TextOp) {
        match parse(msg) {
            ClientMsg::Submit { op_id, base, op } => (op_id, base, op),
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn utf16_offsets_convert_at_astral_chars() {
        let s = "a😀b";
        assert_eq!(utf16_to_char(s, 0), Some(0));
        assert_eq!(utf16_to_char(s, 1), Some(1));
        assert_eq!(utf16_to_char(s, 2), None);
        assert_eq!(utf16_to_char(s, 3), Some(2));
        assert_eq!(utf16_to_char(s, 4), Some(3));
        assert_eq!(utf16_to_char(s, 5), None);
        assert_eq!(char_to_utf16(s, 2), Some(3));
        assert_eq!(char_to_utf16(s, 3), Some(4));
        assert_eq!(char_to_utf16(s, 4), None);
    }

    #[test]
    fn local_replace_builds_submit() {
        let mut c = joined_client("ab");
        let msg = c.local_replace(1, 0, "X").unwrap().unwrap();
        let (_, base, op) = submitted(&msg);
        assert_eq!(base, Seq(0));
        let mut doc = TextDoc::new("ab");
        doc.apply(&op).unwrap();
        assert_eq!(doc.as_str(), "aXb");
        assert_eq!(c.text(), "aXb");
        assert_eq!(c.pending(), 1);
    }

    #[test]
    fn local_replace_converts_utf16_offsets() {
        let mut c = joined_client("a😀b");
        // Replace the emoji (UTF-16 offsets 1..3) with "é".
        let msg = c.local_replace(1, 2, "é").unwrap().unwrap();
        let (_, _, op) = submitted(&msg);
        assert_eq!(op, TextOp::replace(3, 1, 1, "é").unwrap());
        assert_eq!(c.text(), "aéb");
        // An offset inside the surrogate pair is refused and changes nothing.
        let mut c = joined_client("a😀b");
        assert!(c.local_replace(2, 0, "x").is_err());
        assert_eq!(c.text(), "a😀b");
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn remote_edit_reports_utf16_ranges() {
        let mut c = joined_client("ab");
        let op = TextOp::replace(2, 1, 0, "😀").unwrap();
        let update = c
            .receive(&committed(1, other(), OpId(Uuid::from_u128(9)), op))
            .unwrap();
        assert!(update.text_changed);
        assert_eq!(update.text, "a😀b");
        assert_eq!(
            update.edits,
            vec![Edit {
                start_utf16: 1,
                removed_utf16: 0,
                inserted_utf16: 2
            }]
        );
    }

    #[test]
    fn remote_delete_reports_utf16_ranges() {
        let mut c = joined_client("😀a😀b");
        // Delete the second emoji (char 2) and insert "xy" at the end.
        let op = TextOp::builder()
            .retain(2)
            .delete(1)
            .retain(1)
            .insert("xy")
            .build();
        let update = c
            .receive(&committed(1, other(), OpId(Uuid::from_u128(9)), op))
            .unwrap();
        assert_eq!(update.text, "😀abxy");
        assert_eq!(
            update.edits,
            vec![
                Edit {
                    start_utf16: 3,
                    removed_utf16: 2,
                    inserted_utf16: 0
                },
                Edit {
                    start_utf16: 4,
                    removed_utf16: 0,
                    inserted_utf16: 2
                },
            ]
        );
    }

    #[test]
    fn reset_reports_dropped_count() {
        let mut c = joined_client("ab");
        c.local_replace(0, 0, "x").unwrap();
        assert_eq!(c.pending(), 1);
        let update = c
            .receive(&frame(&ServerMsg::Reset {
                epoch: epoch(8),
                seq: Seq(5),
                snapshot: "zz".into(),
                revision: None,
            }))
            .unwrap();
        assert_eq!(update.dropped, 1);
        assert!(update.text_changed);
        assert_eq!(update.text, "zz");
        assert_eq!(update.send, None);
        assert_eq!(c.text(), "zz");
        assert_eq!(c.pending(), 0);
        assert_eq!(c.resend_message().unwrap(), None);
    }

    fn reset(epoch: RoomEpoch, seq: u64, snapshot: &str) -> String {
        frame(&ServerMsg::Reset {
            epoch,
            seq: Seq(seq),
            snapshot: snapshot.into(),
            revision: None,
        })
    }

    fn reset_json(update: &Update) -> serde_json::Value {
        serde_json::to_value(update).unwrap()["reset"].clone()
    }

    #[test]
    fn reset_with_new_epoch_and_different_text_reports_lost_text() {
        // Confirmed text the restarted server never saved, plus a change typed while away.
        let mut c = joined_client("saved, then more");
        c.local_replace(0, 0, "x").unwrap();
        c.join_message().unwrap();
        let update = c.receive(&reset(epoch(8), 0, "saved")).unwrap();
        assert_eq!(update.reset, Some(ResetKind::RestartedLostText));
        assert_eq!(reset_json(&update), "restarted_lost_text");
        assert_eq!(update.dropped, 1);
        assert_eq!(c.text(), "saved");
    }

    #[test]
    fn reset_with_new_epoch_and_same_text_reports_no_loss() {
        let mut c = joined_client("abc");
        c.join_message().unwrap();
        let update = c.receive(&reset(epoch(8), 0, "abc")).unwrap();
        assert_eq!(update.reset, Some(ResetKind::Restarted));
        assert_eq!(reset_json(&update), "restarted");
        assert_eq!(update.dropped, 0);
        assert!(!update.text_changed);
    }

    #[test]
    fn reset_with_same_epoch_reports_resynced() {
        let mut c = joined_client("abc");
        c.local_replace(3, 0, "d").unwrap();
        c.join_message().unwrap();
        let update = c.receive(&reset(epoch(7), 1500, "abc!")).unwrap();
        assert_eq!(update.reset, Some(ResetKind::Resynced));
        assert_eq!(reset_json(&update), "resynced");
        assert_eq!(update.dropped, 1);
        assert_eq!(c.text(), "abc!");
        // A resumed join is not a reset.
        c.join_message().unwrap();
        let update = c.receive(&joined(epoch(7), 1500, None)).unwrap();
        assert_eq!(update.reset, None);
        assert_eq!(reset_json(&update), serde_json::Value::Null);
    }

    #[test]
    fn oversized_change_is_refused_and_state_unchanged() {
        let mut c = joined_client("ab");
        let state = |c: &mut ClientInner| {
            (
                c.text(),
                c.pending(),
                c.resend_message().unwrap(),
                c.join_message().unwrap(),
            )
        };
        let before = state(&mut c);
        c.receive(&joined(epoch(7), 0, None)).unwrap();
        // Too large as typed, and too large once quotes are escaped in the frame.
        for big in [
            "x".repeat(MAX_SUBMIT_BYTES),
            "\"".repeat(MAX_SUBMIT_BYTES / 2 + 1),
        ] {
            let err = c.local_replace(1, 0, &big).unwrap_err();
            assert!(matches!(err, ClientError::TooLarge(_)), "{err:?}");
            assert!(err.to_string().starts_with(TOO_LARGE), "{err}");
            assert_eq!(state(&mut c), before);
            c.receive(&joined(epoch(7), 0, None)).unwrap();
        }
        // A change that fits is still sent whole.
        let fits = "x".repeat(MAX_SUBMIT_BYTES - 1024);
        let msg = c.local_replace(1, 0, &fits).unwrap().unwrap();
        assert!(msg.len() <= MAX_SUBMIT_BYTES);
        assert_eq!(c.text().len(), 2 + fits.len());
    }

    #[test]
    fn join_message_resumes_once_joined() {
        let mut c = ClientInner::new(me());
        assert_eq!(
            parse(&c.join_message().unwrap()),
            ClientMsg::Join {
                client: me(),
                resume: None
            }
        );
        c.receive(&joined(epoch(7), 4, Some("abcd"))).unwrap();
        assert_eq!(
            parse(&c.join_message().unwrap()),
            ClientMsg::Join {
                client: me(),
                resume: Some(Resume {
                    epoch: epoch(7),
                    seq: Seq(4)
                })
            }
        );
    }

    #[test]
    fn resumed_join_resends_change_in_flight() {
        let mut c = joined_client("ab");
        let first = c.local_replace(2, 0, "c").unwrap().unwrap();
        // The connection drops; the page reconnects and joins again.
        c.join_message().unwrap();
        let update = c.receive(&joined(epoch(7), 0, None)).unwrap();
        assert_eq!(update.send.as_deref(), Some(first.as_str()));
        assert_eq!(update.dropped, 0);
        assert!(!update.text_changed);
        assert_eq!(c.text(), "abc");
    }

    #[test]
    fn change_made_before_join_reply_waits_for_it() {
        let mut c = joined_client("ab");
        c.join_message().unwrap();
        // The base of a change sent now could belong to a room that no longer exists.
        assert_eq!(c.local_replace(0, 0, "x").unwrap(), None);
        let update = c.receive(&joined(epoch(7), 0, None)).unwrap();
        let (_, base, op) = submitted(&update.send.unwrap());
        assert_eq!(base, Seq(0));
        assert_eq!(op, TextOp::replace(2, 0, 0, "x").unwrap());
    }

    #[test]
    fn confirmation_sends_next_waiting_change() {
        let mut c = joined_client("");
        let first = c.local_replace(0, 0, "a").unwrap().unwrap();
        assert_eq!(c.local_replace(1, 0, "b").unwrap(), None);
        let (op_id, _, op) = submitted(&first);
        let update = c.receive(&committed(1, me(), op_id, op)).unwrap();
        assert!(!update.text_changed);
        assert!(update.edits.is_empty());
        let (_, base, op) = submitted(&update.send.unwrap());
        assert_eq!(base, Seq(1));
        assert_eq!(op, TextOp::replace(1, 1, 0, "b").unwrap());
        assert_eq!(c.pending(), 1);
    }

    #[test]
    fn out_of_order_commit_is_an_error_and_state_is_unchanged() {
        let mut c = joined_client("ab");
        c.local_replace(0, 0, "x").unwrap();
        let before = (
            c.text(),
            c.pending(),
            c.resend_message().unwrap(),
            c.join_message().unwrap(),
        );
        let op = TextOp::replace(3, 0, 0, "z").unwrap();
        assert!(
            c.receive(&committed(3, other(), OpId(Uuid::from_u128(9)), op))
                .is_err()
        );
        let after = (
            c.text(),
            c.pending(),
            c.resend_message().unwrap(),
            c.join_message().unwrap(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn unparseable_frame_is_an_error() {
        let mut c = joined_client("ab");
        assert!(c.receive("{\"type\":\"nonsense\"}").is_err());
        assert!(c.receive("not json").is_err());
        assert_eq!(c.text(), "ab");
    }

    #[test]
    fn save_state_follows_local_changes_and_checkpoints() {
        let mut c = joined_client("");
        let first = c.local_replace(0, 0, "a").unwrap().unwrap();
        let (op_id, _, op) = submitted(&first);
        let update = c.receive(&committed(1, me(), op_id, op)).unwrap();
        assert_eq!(update.save.unwrap().state, SaveState::Saving);
        let update = c
            .receive(&frame(&ServerMsg::Save {
                state: SaveState::Saved,
                revision: Some("r1".into()),
                through: Seq(1),
            }))
            .unwrap();
        let save = update.save.unwrap();
        assert_eq!(save.state, SaveState::Saved);
        assert_eq!(save.revision.as_deref(), Some("r1"));
        let update = c
            .receive(&frame(&ServerMsg::Save {
                state: SaveState::Failed,
                revision: None,
                through: Seq(1),
            }))
            .unwrap();
        assert_eq!(update.save.unwrap().state, SaveState::Failed);
    }

    #[test]
    fn people_are_listed_with_this_client_marked() {
        let mut c = joined_client("");
        let update = c
            .receive(&frame(&ServerMsg::People {
                participants: vec![
                    protocol::Participant {
                        client: me(),
                        name: "Ann".into(),
                        cursor: None,
                    },
                    protocol::Participant {
                        client: other(),
                        name: "Bo".into(),
                        cursor: Some(1),
                    },
                ],
            }))
            .unwrap();
        let people = update.people.unwrap();
        assert_eq!(
            people
                .iter()
                .map(|p| (p.name.as_str(), p.me))
                .collect::<Vec<_>>(),
            vec![("Ann", true), ("Bo", false)]
        );
    }

    #[test]
    fn refused_change_is_reported_and_next_join_starts_fresh() {
        let mut c = joined_client("ab");
        let first = c.local_replace(0, 0, "x").unwrap().unwrap();
        let (op_id, _, _) = submitted(&first);
        let update = c
            .receive(&frame(&ServerMsg::Refused {
                op_id,
                reason: "no".into(),
            }))
            .unwrap();
        assert_eq!(update.refused.as_deref(), Some("no"));
        assert_eq!(
            parse(&c.join_message().unwrap()),
            ClientMsg::Join {
                client: me(),
                resume: None
            }
        );
        let update = c.receive(&joined(epoch(7), 0, Some("ab"))).unwrap();
        assert_eq!(update.dropped, 1);
        assert_eq!(c.text(), "ab");
    }
}
