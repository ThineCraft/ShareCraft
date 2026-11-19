-- Durable live checkpoints (plan G4). The file bytes stay in Git/the content store; this
-- table ties their revision and hash to one exact live-log position and its opaque editor
-- session data. Verification is recorded later, after an independent writer matches bytes.
create table live_checkpoints (
  document_id uuid not null references documents(id) on delete cascade,
  -- Revision rows are a rebuildable Git projection and may be deleted/reinserted wholesale;
  -- checkpoint facts must survive that reconciliation.
  revision text not null,
  through bigint not null check (through >= 0),
  sha256 bytea not null check (octet_length(sha256) = 32),
  verified_at timestamptz,
  -- The live session whose identity table produced this checkpoint. Ended session detail may
  -- be swept, so this deliberately outlives the live_sessions row.
  session_id uuid not null,
  session_data bytea not null,
  major_reason text,
  major_person uuid references users(id),
  created_at timestamptz not null,
  primary key (document_id, revision),
  unique (document_id, through)
);
create index live_checkpoints_unverified on live_checkpoints (document_id, through)
  where verified_at is null;

-- The small, permanent part of an ended place. It is enough to recognise an expired client
-- after session entries and identity data have completed their retention period.
create table live_return_tombstones (
  session_id uuid not null,
  participant integer not null check (participant >= 1),
  document_id uuid not null references documents(id) on delete cascade,
  client_id uuid not null,
  user_id uuid not null references users(id),
  ended_at timestamptz not null,
  final_seq bigint not null check (final_seq >= 0),
  primary key (session_id, participant)
);
create index live_return_tombstones_by_document_client
  on live_return_tombstones (document_id, client_id);

-- Preserve already-ended sessions when this migration is applied to an active installation.
-- Each session's last position is aggregated in one pass over the entries (live_entries has
-- no session index), never once per ended place.
with last_entries as (
  select session_id, max(seq) as last_seq from live_entries group by session_id
)
insert into live_return_tombstones
    (session_id, participant, document_id, client_id, user_id, ended_at, final_seq)
select s.id, p.number, s.document_id, p.client_id, p.user_id, s.ended_at,
       coalesce(l.last_seq, s.first_seq - 1)
from live_sessions s
join live_participants p on p.session_id = s.id
left join last_entries l on l.session_id = s.id
where s.ended_at is not null;
