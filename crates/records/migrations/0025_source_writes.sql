-- Editable S3 storage: a library's write state, and the writes accepted but not yet in the
-- bucket. Content staged for writing sits in the external-content cache until it is written;
-- a pending row is a reference to it as a cache pin is.
alter table library_sources
  add column write_state text not null default 'ok' check (write_state in ('ok', 'blocked')),
  add column write_error text check (write_error is null or char_length(write_error) <= 300);

create table source_writes (
  node_id uuid primary key references nodes(id),
  -- No cascade: a disconnect is refused while writes are pending.
  library_id uuid not null references library_sources(library_id),
  -- The path below the library's place, as source_items.repository_path writes it.
  path text not null check (char_length(path) between 1 and 1024),
  sha256 text not null references blobs(sha256),
  size bigint not null check (size >= 0),
  revision text not null check (revision ~ '^[0-9a-f]{40}$'),
  -- The tag the change was based on; null: the file is new and must not exist.
  base_tag text,
  by_user uuid not null references users(id),
  state text not null check (state in ('scanning', 'ready', 'failed')),
  error text check (error is null or char_length(error) <= 300),
  attempts integer not null default 0 check (attempts >= 0),
  next_try_at timestamptz,
  flush boolean not null default false,
  first_staged_at timestamptz not null,
  staged_at timestamptz not null
);
create index source_writes_library on source_writes (library_id);
create index source_writes_sha256 on source_writes (sha256);

-- An upload into a library whose bytes live in the bucket reserves no site quota.
alter table upload_sessions add column external boolean not null default false;
