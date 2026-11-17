//! Persistence boundary for the live room.

use std::future::Future;

use collab_core::ids::{ClientId, DocId, UserId};
use records::{Db, DbError, GoneReason, LiveLoaded, LiveLocal, NewLiveChange};
use time::OffsetDateTime;
use uuid::Uuid;

pub(crate) trait LiveStore: Send + Sync + 'static {
    fn load_live(&self, doc: DocId) -> impl Future<Output = Result<LiveLoaded, DbError>> + Send;

    fn start_live_session(
        &self,
        doc: DocId,
        session: Uuid,
        format: &str,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn add_live_participant(
        &self,
        doc: DocId,
        session: Uuid,
        client: ClientId,
        user: UserId,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<u32, DbError>> + Send;

    fn end_live_place(
        &self,
        session: Uuid,
        number: u32,
        reason: GoneReason,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn append_live_change(
        &self,
        doc: DocId,
        change: &NewLiveChange<'_>,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn consume_live_local(
        &self,
        doc: DocId,
        client: ClientId,
        user: UserId,
        local: u64,
        reason: &str,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn live_local(
        &self,
        doc: DocId,
        client: ClientId,
        local: u64,
    ) -> impl Future<Output = Result<LiveLocal, DbError>> + Send;

    fn end_live_session(
        &self,
        session: Uuid,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;
}

impl LiveStore for Db {
    async fn load_live(&self, doc: DocId) -> Result<LiveLoaded, DbError> {
        Db::load_live(self, doc).await
    }

    async fn start_live_session(
        &self,
        doc: DocId,
        session: Uuid,
        format: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::start_live_session(self, doc, session, format, at).await
    }

    async fn add_live_participant(
        &self,
        doc: DocId,
        session: Uuid,
        client: ClientId,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<u32, DbError> {
        Db::add_live_participant(self, doc, session, client, user, at).await
    }

    async fn end_live_place(
        &self,
        session: Uuid,
        number: u32,
        reason: GoneReason,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::end_live_place(self, session, number, reason, at).await
    }

    async fn append_live_change(
        &self,
        doc: DocId,
        change: &NewLiveChange<'_>,
    ) -> Result<(), DbError> {
        Db::append_live_change(self, doc, change).await
    }

    async fn consume_live_local(
        &self,
        doc: DocId,
        client: ClientId,
        user: UserId,
        local: u64,
        reason: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::consume_live_local(self, doc, client, user, local, reason, at).await
    }

    async fn live_local(
        &self,
        doc: DocId,
        client: ClientId,
        local: u64,
    ) -> Result<LiveLocal, DbError> {
        Db::live_local(self, doc, client, local).await
    }

    async fn end_live_session(&self, session: Uuid, at: OffsetDateTime) -> Result<(), DbError> {
        Db::end_live_session(self, session, at).await
    }
}

#[cfg(test)]
pub(crate) use memory::MemStore;

#[cfg(test)]
mod memory {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard};

    use collab_core::ids::Seq;
    use records::{LiveEntryRow, LiveParticipantRow, LiveSessionRow};

    use super::*;

    #[derive(Clone, Default)]
    pub(crate) struct MemStore {
        inner: Arc<Mutex<State>>,
    }

    #[derive(Default)]
    struct State {
        docs: HashMap<DocId, MemoryDoc>,
        fail_next: bool,
    }

    struct MemoryDoc {
        head: Seq,
        last_participant: u32,
        format: Option<String>,
        verified_through: Seq,
        entries: Vec<LiveEntryRow>,
        open_session: Option<LiveSessionRow>,
        clients: HashMap<ClientId, (UserId, u64)>,
        locals: HashMap<(ClientId, u64), LiveLocal>,
    }

    impl Default for MemoryDoc {
        fn default() -> Self {
            Self {
                head: Seq(0),
                last_participant: 0,
                format: None,
                verified_through: Seq(0),
                entries: Vec::new(),
                open_session: None,
                clients: HashMap::new(),
                locals: HashMap::new(),
            }
        }
    }

    impl MemStore {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn fail_next(&self) {
            if let Ok(mut state) = self.inner.lock() {
                state.fail_next = true;
            }
        }

        pub(crate) fn entry_count(&self) -> usize {
            self.inner
                .lock()
                .map(|state| state.docs.values().map(|doc| doc.entries.len()).sum())
                .unwrap_or_default()
        }

        fn state(&self) -> Result<MutexGuard<'_, State>, DbError> {
            self.inner
                .lock()
                .map_err(|_| DbError::new("memory live store lock poisoned"))
        }

        fn write(&self) -> Result<MutexGuard<'_, State>, DbError> {
            let mut state = self.state()?;
            if state.fail_next {
                state.fail_next = false;
                return Err(DbError::new("injected live store failure"));
            }
            Ok(state)
        }
    }

    impl LiveStore for MemStore {
        async fn load_live(&self, doc: DocId) -> Result<LiveLoaded, DbError> {
            let mut state = self.state()?;
            let memory = state.docs.entry(doc).or_default();
            Ok(LiveLoaded {
                head: memory.head,
                last_participant: memory.last_participant,
                format: memory.format.clone(),
                verified_through: memory.verified_through,
                entries: memory.entries.clone(),
                open_session: memory.open_session.clone(),
                clients: memory
                    .clients
                    .iter()
                    .map(|(client, (user, next))| (*client, *user, *next))
                    .collect(),
            })
        }

        async fn start_live_session(
            &self,
            doc: DocId,
            session: Uuid,
            format: &str,
            at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if memory.open_session.is_some() {
                return Err(DbError::new("live session already open"));
            }
            memory.open_session = Some(LiveSessionRow {
                id: session,
                format: format.to_owned(),
                first_seq: Seq(memory.head.0 + 1),
                started_at: at,
                participants: Vec::new(),
            });
            Ok(())
        }

        async fn add_live_participant(
            &self,
            doc: DocId,
            session: Uuid,
            client: ClientId,
            user: UserId,
            at: OffsetDateTime,
        ) -> Result<u32, DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if memory.open_session.as_ref().map(|open| open.id) != Some(session) {
                return Err(DbError::new("live session is not open"));
            }
            memory.last_participant = memory
                .last_participant
                .checked_add(1)
                .ok_or_else(|| DbError::new("participant numbers exhausted"))?;
            let number = memory.last_participant;
            if number == 0 {
                return Err(DbError::new("participant zero is reserved"));
            }
            if let Some(open) = &mut memory.open_session {
                open.participants.push(LiveParticipantRow {
                    number,
                    client,
                    user,
                    joined_at: at,
                    gone: None,
                });
            }
            Ok(number)
        }

        async fn end_live_place(
            &self,
            session: Uuid,
            number: u32,
            reason: GoneReason,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let place = state
                .docs
                .values_mut()
                .filter_map(|doc| doc.open_session.as_mut())
                .find(|open| open.id == session)
                .and_then(|open| open.participants.iter_mut().find(|p| p.number == number))
                .ok_or_else(|| DbError::new("live place does not exist"))?;
            place.gone.get_or_insert(reason);
            Ok(())
        }

        async fn append_live_change(
            &self,
            doc: DocId,
            change: &NewLiveChange<'_>,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if change.seq.0 != memory.head.0 + 1 {
                return Err(DbError::new("change does not follow live head"));
            }
            if !matches!(
                memory.locals.get(&(change.client, change.local)),
                None | Some(LiveLocal::Unseen)
            ) {
                return Err(DbError::new("local already used"));
            }
            memory.head = change.seq;
            memory.format = Some(change.format.to_owned());
            memory.entries.push(LiveEntryRow::Change {
                seq: change.seq,
                session: change.session,
                client: change.client,
                local: change.local,
                participant: change.participant,
                user: change.user,
                seen: change.seen,
                at: change.at,
                seed: change.seed,
                format: change.format.to_owned(),
                payload: change.payload.to_vec(),
                witnesses: change.witnesses.to_vec(),
                cancelled_by: None,
            });
            memory.locals.insert(
                (change.client, change.local),
                LiveLocal::Stored { seq: change.seq },
            );
            let next = change.local.saturating_add(1);
            memory
                .clients
                .entry(change.client)
                .and_modify(|(_, current)| *current = (*current).max(next))
                .or_insert((change.user, next));
            Ok(())
        }

        async fn consume_live_local(
            &self,
            doc: DocId,
            client: ClientId,
            user: UserId,
            local: u64,
            reason: &str,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            memory
                .locals
                .entry((client, local))
                .or_insert_with(|| LiveLocal::Refused {
                    reason: reason.to_owned(),
                });
            let next = local.saturating_add(1);
            memory
                .clients
                .entry(client)
                .and_modify(|(_, current)| *current = (*current).max(next))
                .or_insert((user, next));
            Ok(())
        }

        async fn live_local(
            &self,
            doc: DocId,
            client: ClientId,
            local: u64,
        ) -> Result<LiveLocal, DbError> {
            Ok(self
                .state()?
                .docs
                .get(&doc)
                .and_then(|memory| memory.locals.get(&(client, local)))
                .cloned()
                .unwrap_or(LiveLocal::Unseen))
        }

        async fn end_live_session(
            &self,
            session: Uuid,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state
                .docs
                .values_mut()
                .find(|doc| doc.open_session.as_ref().map(|open| open.id) == Some(session))
                .ok_or_else(|| DbError::new("live session does not exist"))?;
            memory.open_session = None;
            Ok(())
        }
    }
}
