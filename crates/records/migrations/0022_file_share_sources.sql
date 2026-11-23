-- File-share sources use the connected-library machinery, but keep their content outside the
-- content store until first use. The external metadata below is enough to detect a change and
-- to name a stable ShareCraft revision without treating the share's path as content.
alter table library_sources drop constraint library_sources_kind_check;
alter table library_sources add constraint library_sources_kind_check
  check (kind in ('github', 'share'));
alter table library_sources
  add column location text check (char_length(location) <= 1000),
  add constraint library_sources_kind_location_check
    check ((kind = 'share') = (location is not null)),
  add column mode text not null default 'read_only'
    check (mode in ('read_write', 'read_only')),
  add constraint library_sources_kind_mode_check
    check (kind = 'share' or mode = 'read_only'),
  add column next_check_at timestamptz,
  add column hidden_detail jsonb not null default '{}';

alter table source_items
  add column revision text check (revision ~ '^[0-9a-f]{40}$'),
  add column modified_at timestamptz,
  add column content_sha256 text check (content_sha256 ~ '^[0-9a-f]{64}$'),
  add column read_at timestamptz,
  add column unavailable text
    check (unavailable in ('denied', 'gone', 'too_large', 'locked')),
  add column missing_since timestamptz;
create index source_items_content_sha256 on source_items (content_sha256)
  where content_sha256 is not null;

create table source_cache (
  sha256 text primary key references blobs(sha256),
  size bigint not null check (size >= 0),
  last_used_at timestamptz not null default now(),
  created_at timestamptz not null default now()
);
create index source_cache_last_used_at on source_cache (last_used_at);
