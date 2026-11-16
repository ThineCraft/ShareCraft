-- Connected libraries (GitHub surfacing). A library with a row in library_sources mirrors a
-- branch of a repository: its folders and files are ordinary nodes, and source_items records
-- which node holds which repository path and Git blob. The connection is an attribute of the
-- library, not a kind of its own.
--
-- Secrets (a repository token, a webhook secret) are kept only as AES-256-GCM ciphertext with
-- the nonce and the id of the key that sealed them; the key itself is never in the database.
create table secrets (
  id uuid primary key,
  key_id integer not null,
  nonce bytea not null check (octet_length(nonce) = 12),
  ciphertext bytea not null check (octet_length(ciphertext) > 0),
  created_at timestamptz not null default now()
);

create table library_sources (
  library_id uuid primary key references libraries(id),
  kind text not null check (kind in ('github')),
  owner text not null,
  repo text not null,
  branch text not null,
  -- A folder of the repository to mirror; empty for the whole repository.
  folder text not null default '',
  token_secret uuid references secrets(id),
  webhook_secret uuid references secrets(id),
  poll_seconds integer not null check (poll_seconds between 60 and 86400),
  connected_by uuid not null references users(id),
  state text not null default 'connecting'
    check (state in ('connecting', 'ok', 'syncing', 'not_connected', 'rate_limited', 'failed')),
  -- The sync bookmark: the commit last mirrored and the entity tag of the last branch answer.
  last_commit text,
  etag text,
  last_checked_at timestamptz,
  last_synced_at timestamptz,
  -- A plain sentence of our own; nothing from a response body is stored.
  last_error text check (last_error is null or char_length(last_error) <= 300),
  retry_at timestamptz,
  files bigint not null default 0 check (files >= 0),
  bytes bigint not null default 0 check (bytes >= 0),
  -- Entries that were not mirrored (links, submodules, over-long or clashing names).
  hidden bigint not null default 0 check (hidden >= 0)
);
create index library_sources_due on library_sources (last_checked_at)
  where state in ('connecting', 'ok', 'rate_limited', 'failed');

create table source_items (
  library_id uuid not null references library_sources(library_id),
  path text not null,
  node_id uuid not null unique references nodes(id),
  -- Null for a folder.
  blob_sha text,
  size bigint not null default 0 check (size >= 0),
  primary key (library_id, path)
);
