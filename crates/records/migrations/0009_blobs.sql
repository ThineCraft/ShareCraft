-- The content store's safety rows. Each stored content (a file named by its SHA-256 under the
-- data folder) has one row in `blobs`; each version that names it, or is about to, has one row
-- in `blob_refs`. A pending row is written, with the blob's row locked FOR SHARE, before the
-- Git commit that names the hash, and completed with the revision (or deleted) after it.
-- Erasure locks the blob's row FOR UPDATE, counts every row naming it, pending ones included,
-- and erases only at zero. Git stays the authority: these rows can be rebuilt from it.
create table blobs (
  sha256 text primary key check (sha256 ~ '^[0-9a-f]{64}$'),
  size bigint not null check (size >= 0),
  created_at timestamptz not null default now()
);

-- A completed row names its revision; a pending one has none yet. The identity column names a
-- claim, so a pending row can be settled whatever else is written meanwhile.
create table blob_refs (
  id bigint generated always as identity primary key,
  sha256 text not null references blobs(sha256),
  doc_id uuid not null,
  revision text,
  pending boolean not null,
  created_at timestamptz not null default now(),
  check (pending = (revision is null)),
  unique (doc_id, sha256, revision)
);
create index blob_refs_sha256 on blob_refs (sha256);
create index blob_refs_pending on blob_refs (created_at) where pending;

-- Flags that ask the next start to rebuild `blob_refs` from Git: `blob_refs_upgrade`, set here
-- once for the upgrade to the content store, and `blob_refs_dirty`, set whenever a completed
-- row could not be written. A rebuild with no failure clears them.
create table maintenance_flags (
  name text primary key check (name in ('blob_refs_upgrade', 'blob_refs_dirty')),
  set_at timestamptz not null default now()
);
insert into maintenance_flags (name) values ('blob_refs_upgrade');
