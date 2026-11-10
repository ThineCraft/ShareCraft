//! Writing events from inside rooms. A room never waits for the database: it hands each event
//! to an [`EventSink`], and one task appends them to the event log in the order they were sent.

use records::{Db, NewEvent};
use tokio::sync::mpsc;

/// Longest error text kept in an event, in characters.
const MAX_ERROR_CHARS: usize = 500;

/// Where rooms send the events they cause. Cloning shares the one writer task.
#[derive(Clone, Debug)]
pub(crate) struct EventSink {
    /// `None` for a sink that drops everything (a room built without a database).
    queue: Option<mpsc::UnboundedSender<NewEvent>>,
}

impl EventSink {
    /// Starts the task that appends events to `db`, and returns the sink that feeds it. The task
    /// ends once every clone of the sink is dropped and what was sent is written. An event that
    /// cannot be written is logged and dropped.
    pub(crate) fn start(db: Db) -> Self {
        let (queue, mut events) = mpsc::unbounded_channel::<NewEvent>();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let Err(error) = db.append_event(&event).await {
                    eprintln!(
                        "sharecraft-server: could not record a {} event: {error}",
                        event.kind
                    );
                }
            }
        });
        Self { queue: Some(queue) }
    }

    /// A sink and the receiver that reads what is emitted into it, for tests of rooms.
    #[cfg(test)]
    pub(crate) fn channel() -> (Self, mpsc::UnboundedReceiver<NewEvent>) {
        let (queue, events) = mpsc::unbounded_channel();
        (Self { queue: Some(queue) }, events)
    }

    /// A sink that drops every event.
    pub(crate) fn discard() -> Self {
        Self { queue: None }
    }

    /// Queues `event` for writing. Never waits.
    pub(crate) fn emit(&self, event: NewEvent) {
        if let Some(queue) = &self.queue {
            // An error means the writer task is gone; the event is dropped like any other
            // that cannot be written.
            let _ = queue.send(event);
        }
    }
}

/// `error` as event detail text: cut to a fixed length, so that no failure can fill the log.
pub(crate) fn error_text(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}
