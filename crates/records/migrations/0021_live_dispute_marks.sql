-- Bounded checkpoint disputes (plan G4, Task 11 re-review). A person may take part in one
-- dispute that stops a document's live session per hour; one who takes part in another within
-- the hour is excluded from writing and verifying that document's checkpoints for a day.
create table live_dispute_marks (
  document_id uuid not null references documents(id) on delete cascade,
  user_id uuid not null references users(id),
  -- When a dispute this person took part in last stopped the live session.
  stopped_at timestamptz,
  -- Until when this person writes and verifies none of the document's checkpoints.
  excluded_until timestamptz,
  primary key (document_id, user_id)
);
