-- Resumable upload sessions. A session uploads one file, a chunk at a time, into its part file
-- (<data>/uploads/<id>.part); `received` is how many bytes of it are written and flushed. It
-- makes either a new file in `parent_id` or a new version of the file document `doc_id`, made
-- from `base_revision`. `sha256` and `size` are what the uploader declared; finishing checks the
-- bytes against them. A session with no chunk since `touched_at` plus a day is swept.
create table upload_sessions (
  id uuid primary key,
  owner uuid not null references users(id),
  site_id uuid not null references nodes(id),
  parent_id uuid references nodes(id),
  doc_id uuid references nodes(id),
  name text not null,
  size bigint not null check (size > 0),
  sha256 bytea not null check (octet_length(sha256) = 32),
  received bigint not null default 0 check (received >= 0 and received <= size),
  base_revision text,
  created_at timestamptz not null default now(),
  touched_at timestamptz not null default now(),
  check ((parent_id is null) <> (doc_id is null)),
  check (doc_id is not null or base_revision is null)
);
create index upload_sessions_owner on upload_sessions (owner);
create index upload_sessions_touched on upload_sessions (touched_at);
