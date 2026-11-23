-- S3 sources: a library connected to a bucket (and optional prefix) of an S3-compatible
-- service. Like a file share, its items are real tree nodes whose content stays outside the
-- content store until first use. The sealed secret key lives in library_sources.token_secret;
-- where the bucket is and how it behaved when probed live in source_s3, and every request made
-- on the library's behalf is counted per month in source_usage.
alter table library_sources drop constraint library_sources_kind_check;
alter table library_sources add constraint library_sources_kind_check
  check (kind in ('github', 'share', 's3'));
alter table library_sources drop constraint library_sources_kind_location_check;
alter table library_sources add constraint library_sources_kind_location_check
  check ((kind in ('share', 's3')) = (location is not null));
alter table library_sources drop constraint library_sources_kind_mode_check;
alter table library_sources add constraint library_sources_kind_mode_check
  check (kind in ('share', 's3') or mode = 'read_only');

create table source_s3 (
  library_id uuid primary key references library_sources(library_id) on delete cascade,
  -- The name of an approved endpoint, not its address.
  endpoint text not null check (char_length(endpoint) <= 60),
  bucket text not null check (char_length(bucket) between 3 and 63),
  -- Empty, or a clean prefix ending in a slash.
  prefix text not null default '' check (char_length(prefix) <= 512),
  access_key_id text not null check (char_length(access_key_id) between 1 and 128),
  versioning text not null default 'unknown'
    check (versioning in ('enabled', 'suspended', 'none', 'unknown')),
  conditional text not null default 'unknown' check (conditional in ('yes', 'no', 'unknown')),
  -- Monthly request and byte budgets; null means the server's default.
  budget_list bigint,
  budget_read bigint,
  budget_bytes bigint
);

create table source_usage (
  library_id uuid not null references library_sources(library_id) on delete cascade,
  -- The first day of the month the counts belong to.
  month date not null,
  list_requests bigint not null default 0 check (list_requests >= 0),
  read_requests bigint not null default 0 check (read_requests >= 0),
  write_requests bigint not null default 0 check (write_requests >= 0),
  bytes_down bigint not null default 0 check (bytes_down >= 0),
  bytes_up bigint not null default 0 check (bytes_up >= 0),
  cache_hits bigint not null default 0 check (cache_hits >= 0),
  warned boolean not null default false,
  primary key (library_id, month)
);

-- An archived object (one the store keeps offline) cannot be read until it is restored. The
-- check added by the file-share migration has no name of its own, so it is found by what it
-- constrains.
do $$
declare
  old_check text;
begin
  for old_check in
    select conname from pg_constraint
    where conrelid = 'source_items'::regclass
      and contype = 'c'
      and pg_get_constraintdef(oid) like '%unavailable%'
  loop
    execute format('alter table source_items drop constraint %I', old_check);
  end loop;
end
$$;
alter table source_items add constraint source_items_unavailable_check
  check (unavailable in ('denied', 'gone', 'too_large', 'locked', 'archived'));
-- The object version a read was made against, where the bucket keeps versions.
alter table source_items
  add column version_id text check (char_length(version_id) <= 1024);
