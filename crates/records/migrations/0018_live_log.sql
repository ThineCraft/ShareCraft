-- The live session log (plan G2). A live document's changes are put in one order by the live
-- room, stored here before anyone is sent them, and kept until a verified revision covers
-- them (then for 30 days). The server never reads a change's content: `payload` holds the
-- bytes exactly as they came. Numbered 0018 because 0017 is taken by another branch.

-- One row per document that has ever had a live session.
create table live_logs (
  document_id uuid primary key references documents(id) on delete cascade,
  -- The newest position ever given; never goes back, even when entries are swept.
  head bigint not null default 0 check (head >= 0),
  -- The newest participant number ever given; numbers are never reused within a document.
  last_participant integer not null default 0 check (last_participant >= 0),
  -- The format of the kept entries; null when none.
  format text,
  -- Set by G4: entries at or below this position are covered by a verified revision.
  verified_through bigint not null default 0
    check (verified_through >= 0 and verified_through <= head),
  -- When verified_through was last moved; null while nothing is verified.
  verified_at timestamptz);

-- A live session: from the first person joining to the last one leaving.
create table live_sessions (
  id uuid primary key,
  document_id uuid not null references documents(id) on delete cascade,
  -- The format every change in the session has.
  format text not null,
  -- The first position the session could give: the head when it started, plus one.
  first_seq bigint not null check (first_seq >= 1),
  started_at timestamptz not null,
  -- Null while the session is open.
  ended_at timestamptz);
create unique index live_sessions_one_open on live_sessions (document_id) where ended_at is null;

-- A place in a session: one client under one participant number, from 1 and never 0.
create table live_participants (
  session_id uuid not null references live_sessions(id) on delete cascade,
  number integer not null check (number >= 1),
  client_id uuid not null,
  user_id uuid not null references users(id),
  joined_at timestamptz not null,
  -- When and why the place ended; both null while it is held.
  gone_at timestamptz,
  gone_reason text check (gone_reason in ('left','expired','removed','cannot_open')),
  check ((gone_at is null) = (gone_reason is null)),
  primary key (session_id, number));

-- Each client of a document, the person it belongs to, and one past the highest local number
-- stored or finally refused. It never goes back; local numbers may skip forwards, while an
-- unseen number below it is out of order.
create table live_clients (
  document_id uuid not null references documents(id) on delete cascade,
  client_id uuid not null,
  user_id uuid not null references users(id),
  next_local bigint not null default 1 check (next_local >= 1),
  primary key (document_id, client_id));

-- The log: a change from a client, or a cancel record naming the change it cancels.
create table live_entries (
  document_id uuid not null references documents(id) on delete cascade,
  -- The position in the document's one order, from 1, without gaps or repeats.
  seq bigint not null check (seq >= 1),
  session_id uuid not null references live_sessions(id),
  kind text not null check (kind in ('change','cancel')),
  -- A change's sender: its client and that client's own number for the change, its
  -- participant number in the session (from 1), and its person. All null for a cancel.
  client_id uuid, local bigint, participant integer, user_id uuid references users(id),
  -- The position the sender had seen when making the change, as stated; 0 for a cancel.
  seen bigint not null default 0,
  -- The server's stamp: its clock, and 32 random bits (0 to 4294967295).
  at timestamptz not null,
  seed bigint not null check (seed between 0 and 4294967295),
  format text not null,
  -- A change's content, byte for byte (UTF-8 text); emptied when the change is cancelled.
  payload bytea not null default '',
  -- The participant numbers holding a place with editing rights when the change was stored.
  witnesses integer[] not null default '{}',
  -- A cancel record's cancelled change, and how many reports cancelled it.
  cancels bigint, reports integer,
  -- A change's cancel record, once cancelled.
  cancelled_by bigint check (cancelled_by > seq),
  check (seen >= 0 and seen < seq),
  check ((kind = 'change' and client_id is not null and local >= 1 and participant >= 1
          and user_id is not null and cancels is null and reports is null)
      or (kind = 'cancel' and client_id is null and local is null and participant is null
          and user_id is null and cancels >= 1 and cancels < seq and reports >= 1
          and cancelled_by is null)),
  primary key (document_id, seq));
create unique index live_entries_by_client on live_entries (document_id, client_id, local)
  where kind = 'change';

-- A client's local numbers whose change was finally refused, so that a resent refused number
-- is refused again, also after the room restarts. A number is stored or refused, never both.
create table live_refusals (
  document_id uuid not null references documents(id) on delete cascade,
  client_id uuid not null,
  local bigint not null check (local >= 1),
  -- The reason as the wire names it, such as read_only.
  reason text not null check (reason ~ '^[a-z_]{1,32}$'),
  at timestamptz not null,
  primary key (document_id, client_id, local));
