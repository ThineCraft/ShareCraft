-- Editable S3 storage, slice 2: what a library holds in its holding folder for deleted bin
-- entries, and the multipart uploads in progress. Moved paths need no table of their own.

create table source_held (
  entry_id uuid primary key references nodes(id),           -- the bin entry
  library_id uuid not null references library_sources(library_id) on delete cascade,
  root text not null check (char_length(root) between 1 and 1024),  -- library-relative, ends '/'
  objects bigint not null check (objects >= 0),
  held_at timestamptz not null,
  retry_at timestamptz
);
create index source_held_library on source_held (library_id);

create table source_multipart (
  library_id uuid not null references library_sources(library_id),  -- no cascade: a disconnect is refused while one is open
  upload_id text not null check (char_length(upload_id) between 1 and 1024),
  node_id uuid not null references nodes(id),
  key text not null check (char_length(key) between 1 and 1024),
  started_at timestamptz not null,
  primary key (library_id, upload_id)
);
